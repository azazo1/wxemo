//! wgpu 主机侧: 设备初始化, 缓冲布局与单批提交流水线.
//!
//! 缓冲布局与 `kernel.wgsl` 的绑定一一对应:
//!
//! | 绑定 | 缓冲 | 内容 |
//! | --- | --- | --- |
//! | 0 | `params` | 批首 seed, 候选数, 账号标识数, 样本数, 每个账号标识的 tail 长度 |
//! | 1 | `tables` | AES 正向 S 盒 (前 256 项) 与逆 S 盒 (后 256 项) |
//! | 2 | `tails` | 每个账号标识的 "账号标识 + EMOTICON" 字节, 固定步长 |
//! | 3 | `prefixes` | 每个样本密文首块的 4 个大端序字 |
//! | 4 | `result_count` | 命中计数的原子量, 兼作批内提前退出标志 |
//! | 5 | `result_indices` | 命中的候选序号 |
//!
//! 结果通过一个 MAP_READ 的暂存缓冲读回, 用 `map_async` 加轮询推进,
//! 不在每批都做阻塞式 `poll(Wait)`, 以免把一个同步点硬塞进提交路径.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tracing::{debug, info};
use wgpu::util::DeviceExt;

use crate::emoticon::search::SearchSpec;

/// 着色器的工作组大小, 必须与 `kernel.wgsl` 的 `@workgroup_size` 一致.
pub const WORKGROUP: u32 = 128;

/// 单次 dispatch 允许的最大工作组数.
const MAX_GROUPS: u32 = 65535;

/// 每批提交的候选数上限: 一次 dispatch 的工作组数不能超过 MAX_GROUPS.
pub const BATCH_CANDIDATES: u32 = MAX_GROUPS * WORKGROUP;

/// 结果槽位数量, 与内核的 `MAX_HITS` 一致.
pub const MAX_HITS: usize = 64;

/// 每个账号标识的 tail 在缓冲里占用的 u32 数, 与内核的 `TAIL_STRIDE` 一致.
const TAIL_STRIDE: usize = 64;

/// 缓冲允许的账号标识数量上限, 与内核的 `tails` 长度一致.
const MAX_WXIDS: usize = 8;

/// 缓冲允许的样本数量上限, 与内核的 `prefixes` 长度一致.
const MAX_SAMPLES: usize = 16;

/// 单条候选消息的长度上限: 超过 55 字节就无法只用一个 MD5 块.
const MAX_GPU_MESSAGE: usize = 55;

/// 暂存缓冲大小: 4 字节计数加 64 个结果槽位, 再向上取整到 16 的倍数.
const STAGING_BYTES: u64 = 512;

/// 轮询 `map_async` 回调的间隔.
const POLL_INTERVAL: Duration = Duration::from_micros(200);

/// 传给着色器的参数块, 布局必须与 `kernel.wgsl` 的 `Params` 一致.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Params {
    base: u32,
    work_count: u32,
    wxid_count: u32,
    sample_count: u32,
    tail_lengths: [u32; MAX_WXIDS],
}

/// 初始化好的 wgpu 计算资源.
pub struct Kernel {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    bind_group: wgpu::BindGroup,
    params: wgpu::Buffer,
    result_count: wgpu::Buffer,
    result_indices: wgpu::Buffer,
    staging: wgpu::Buffer,
    wxid_count: u32,
    sample_count: u32,
    tail_lengths: [u32; MAX_WXIDS],
    adapter_name: String,
    backend_name: String,
    device_type: String,
}

/// 枚举当前能看到的 wgpu adapter 数量.
///
/// 既用于判断后端是否可用, 也供测试区分 "这台机器没有 GPU" (跳过)
/// 与 "内核有问题" (失败), 两者含义完全不同.
pub fn adapter_count() -> usize {
    pollster::block_on(wgpu::Instance::default().enumerate_adapters(SEARCH_BACKENDS)).len()
}

/// 搜后端时只看 wgpu 的一线支持.
///
/// 排除 GL: GL 的实际实现常常是 Mesa / llvmpipe 这类软件回退, 选中它只会更慢.
/// 注意这不解决 Vulkan 枚举慢的问题 —— 实测某台 Windows 机器上 Vulkan 枚举要
/// 14.4 秒 (总加载的 98%), 而 PRIMARY 包含 Vulkan. 那是驱动栈的问题, 见使用说明.
const SEARCH_BACKENDS: wgpu::Backends = wgpu::Backends::PRIMARY;

impl Kernel {
    /// 探测 adapter, 编译内核并建好全部缓冲.
    ///
    /// 任何一步失败都返回错误, 由调用方决定是回落 CPU 还是直接报错.
    pub fn new(spec: &SearchSpec) -> Result<Self> {
        let started = std::time::Instant::now();
        let wxid_count = spec.wxids.len();
        let sample_count = spec.samples.len();
        if wxid_count == 0 {
            bail!("没有账号标识, 无法在 GPU 上搜索");
        }
        if wxid_count > MAX_WXIDS {
            bail!("账号标识有 {wxid_count} 个, 超过 GPU 缓冲上限 {MAX_WXIDS}");
        }
        if sample_count == 0 {
            bail!("没有判定样本, 无法在 GPU 上搜索");
        }
        if sample_count > MAX_SAMPLES {
            bail!("样本有 {sample_count} 个, 超过 GPU 缓冲上限 {MAX_SAMPLES}");
        }

        let mut tail_lengths = [0u32; MAX_WXIDS];
        let mut tails = vec![0u32; MAX_WXIDS * TAIL_STRIDE];
        for (index, wxid) in spec.wxids.iter().enumerate() {
            let mut tail = wxid.to_vec();
            tail.extend_from_slice(b"EMOTICON");
            if tail.len() > TAIL_STRIDE {
                bail!(
                    "账号标识 {} 字节, 加上后缀超出 GPU 的 {} 字节缓冲",
                    wxid.len(),
                    TAIL_STRIDE - 8
                );
            }
            // 最坏情况 seed 有 10 位十进制, 消息总长必须能装进一个 MD5 块.
            if 10 + tail.len() > MAX_GPU_MESSAGE {
                bail!(
                    "账号标识 {} 字节, 消息最长 {} 字节, 超过单块 MD5 的 {} 字节上限",
                    wxid.len(),
                    10 + tail.len(),
                    MAX_GPU_MESSAGE
                );
            }
            for (offset, byte) in tail.iter().enumerate() {
                tails[index * TAIL_STRIDE + offset] = *byte as u32;
            }
            tail_lengths[index] = tail.len() as u32;
        }

        // 每个样本的密文首块按大端序打成 4 个字, 与 AES 的列序一致.
        if spec.samples.iter().any(|sample| sample.prefix.len() < 16) {
            bail!("有样本的密文首块不足 16 字节");
        }
        let mut prefixes = vec![0u32; MAX_SAMPLES * 4];
        for (index, sample) in spec.samples.iter().enumerate() {
            for column in 0..4 {
                let bytes = &sample.prefix[column * 4..column * 4 + 4];
                prefixes[index * 4 + column] =
                    u32::from_be_bytes(bytes.try_into().expect("切片长度固定"));
            }
        }

        let instance = wgpu::Instance::new({
            let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
            desc.backends = SEARCH_BACKENDS;
            desc
        });
        let adapters = pollster::block_on(instance.enumerate_adapters(SEARCH_BACKENDS));
        if adapters.is_empty() {
            bail!("没有枚举到任何 wgpu adapter");
        }
        let mut chosen = None;
        for adapter in &adapters {
            debug!(
                name = %adapter.get_info().name,
                kind = ?adapter.get_info().device_type,
                backend = ?adapter.get_info().backend,
                "发现 adapter"
            );
        }
        for adapter in adapters {
            let is_discrete = adapter.get_info().device_type == wgpu::DeviceType::DiscreteGpu;
            if chosen.is_none() || is_discrete {
                chosen = Some(adapter);
            }
            if is_discrete {
                break;
            }
        }
        let adapter = chosen.expect("adapters 非空");
        let info = adapter.get_info();
        debug!(ms = started.elapsed().as_millis(), "adapter 枚举完成");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("wxemo-seed-search"),
            ..Default::default()
        }))
        .context("请求 wgpu 设备失败")?;
        debug!(ms = started.elapsed().as_millis(), "设备创建完成");

        // 着色器编译与管线创建的错误通过错误作用域捕获, 不让它直接 panic.
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("wxemo-seed-search-kernel"),
            source: wgpu::ShaderSource::Wgsl(include_str!("kernel.wgsl").into()),
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("wxemo-seed-search"),
            entries: &[
                storage_entry(0, true),
                storage_entry(1, true),
                storage_entry(2, true),
                storage_entry(3, true),
                storage_entry(4, false),
                storage_entry(5, false),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wxemo-seed-search"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("wxemo-seed-search"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        if let Some(error) = pollster::block_on(scope.pop()) {
            bail!("wgpu 着色器或管线创建失败: {error}");
        }
        debug!(
            ms = started.elapsed().as_millis(),
            "着色器编译与管线创建完成"
        );

        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wxemo-params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let tables = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("wxemo-tables"),
            contents: &by_u32_bytes(&sbox_tables()),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let tails = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("wxemo-tails"),
            contents: &by_u32_bytes(&tails),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let prefixes = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("wxemo-prefixes"),
            contents: &by_u32_bytes(&prefixes),
            usage: wgpu::BufferUsages::STORAGE,
        });
        let result_count = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wxemo-result-count"),
            size: 4,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let result_indices = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wxemo-result-indices"),
            size: (MAX_HITS * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wxemo-staging"),
            size: STAGING_BYTES,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("wxemo-seed-search"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: tables.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: tails.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: prefixes.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: result_count.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: result_indices.as_entire_binding(),
                },
            ],
        });

        // 参数与结果缓冲只需要一次性准备好静态部分, 动态部分每批写.
        queue.write_buffer(
            &params,
            0,
            &by_u32_bytes(&param_words(
                wxid_count as u32,
                sample_count as u32,
                0,
                0,
                &tail_lengths,
            )),
        );
        queue.write_buffer(&result_count, 0, &0u32.to_ne_bytes());

        info!(
            adapter = %info.name,
            backend = ?info.backend,
            device_type = ?info.device_type,
            wxids = wxid_count,
            samples = sample_count,
            init_ms = started.elapsed().as_millis(),
            "wgpu 后端就绪"
        );

        Ok(Self {
            device,
            queue,
            pipeline,
            bind_group,
            params,
            result_count,
            result_indices,
            staging,
            wxid_count: wxid_count as u32,
            sample_count: sample_count as u32,
            tail_lengths,
            adapter_name: info.name.clone(),
            backend_name: format!("{:?}", info.backend),
            device_type: format!("{:?}", info.device_type),
        })
    }

    /// 实际使用的 adapter 名称.
    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    /// 实际使用的图形后端名称.
    pub fn backend_name(&self) -> &str {
        &self.backend_name
    }

    /// adapter 的类别, 例如集成或独立显卡.
    pub fn device_type(&self) -> &str {
        &self.device_type
    }

    /// 每批能覆盖的候选数, 受单次 dispatch 的工作组数上限约束.
    pub fn batch_capacity(&self) -> u32 {
        BATCH_CANDIDATES.min(MAX_GROUPS * WORKGROUP)
    }

    /// 提交一批候选并读回命中的候选序号, 返回值的升序即 (seed, 账号标识) 的字典序.
    pub fn dispatch(&self, base: u32, work_count: u32) -> Result<Vec<u32>> {
        if work_count == 0 {
            return Ok(Vec::new());
        }
        let groups = work_count.div_ceil(WORKGROUP);
        if groups > MAX_GROUPS {
            bail!("单批 {work_count} 个候选需要 {groups} 个工作组, 超过上限 {MAX_GROUPS}");
        }

        // 1. 复位本批的参数与结果计数.
        self.queue.write_buffer(
            &self.params,
            0,
            &by_u32_bytes(&param_words(
                self.wxid_count,
                self.sample_count,
                base,
                work_count,
                &self.tail_lengths,
            )),
        );
        self.queue
            .write_buffer(&self.result_count, 0, &0u32.to_ne_bytes());

        // 2. 记录计算与读回命令.
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("wxemo-seed-search"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("wxemo-seed-search"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.dispatch_workgroups(groups, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&self.result_count, 0, &self.staging, 0, 4);
        encoder.copy_buffer_to_buffer(
            &self.result_indices,
            0,
            &self.staging,
            4,
            (MAX_HITS * 4) as u64,
        );
        self.queue.submit([encoder.finish()]);

        // 3. 异步映射加轮询, 而不是阻塞在 Maintain::Wait.
        //
        // 无论走哪条分支都要把缓冲解除映射, 否则下一批的 map_async 会直接 panic.
        let state = Arc::new(AtomicU8::new(0));
        let signal = state.clone();
        self.staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                signal.store(if result.is_ok() { 1 } else { 2 }, Ordering::SeqCst);
            });

        let outcome;
        loop {
            if let Err(error) = self.device.poll(wgpu::PollType::Poll) {
                outcome = Err(anyhow!("轮询 wgpu 设备失败: {error}"));
                break;
            }
            match state.load(Ordering::SeqCst) {
                0 => std::thread::sleep(POLL_INTERVAL),
                1 => {
                    outcome = parse_hits(&self.staging);
                    break;
                }
                _ => {
                    outcome = Err(anyhow!("读回 wgpu 结果缓冲失败"));
                    break;
                }
            }
        }
        self.staging.unmap();
        let mut hits = outcome?;
        hits.sort_unstable();
        Ok(hits)
    }
}

/// 解析暂存缓冲: 前 4 字节是命中计数, 之后是各个命中槽位.
fn parse_hits(staging: &wgpu::Buffer) -> Result<Vec<u32>> {
    let view = staging
        .slice(..)
        .get_mapped_range()
        .map_err(|error| anyhow!("映射 wgpu 结果缓冲失败: {error}"))?;
    let count = u32::from_ne_bytes(view[0..4].try_into().expect("前 4 字节存在"));
    let wanted = (count as usize).min(MAX_HITS);
    let mut hits = Vec::with_capacity(wanted);
    for slot in 0..wanted {
        let start = 4 + slot * 4;
        let work = u32::from_ne_bytes(view[start..start + 4].try_into().expect("槽位长度固定"));
        // 结果槽位是递增分配的, 但为了稳妥仍然去重.
        if !hits.contains(&work) {
            hits.push(work);
        }
    }
    debug!(count, slots = hits.len(), "读回一批结果");
    Ok(hits)
}

/// 把参数块序列化成字节.
fn param_words(
    wxid_count: u32,
    sample_count: u32,
    base: u32,
    work_count: u32,
    tail_lengths: &[u32],
) -> Vec<u32> {
    let mut words = Vec::with_capacity(4 + MAX_WXIDS);
    words.push(base);
    words.push(work_count);
    words.push(wxid_count);
    words.push(sample_count);
    for index in 0..MAX_WXIDS {
        words.push(*tail_lengths.get(index).unwrap_or(&0));
    }
    words
}

/// 把 u32 序列化成原生字节序.
fn by_u32_bytes(words: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for word in words {
        bytes.extend_from_slice(&word.to_ne_bytes());
    }
    bytes
}

/// 生成一个只读存储缓冲的布局项.
fn storage_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// 生成 AES 的 S 盒与逆 S 盒, 与 `kernel.wgsl` 里注释列出的表一致.
fn sbox_tables() -> [u32; 512] {
    let mut tables = [0u32; 512];
    for value in 0..256u32 {
        let inverse = gf_inverse(value as u8);
        let forward = inverse
            ^ rotl8(inverse, 1)
            ^ rotl8(inverse, 2)
            ^ rotl8(inverse, 3)
            ^ rotl8(inverse, 4)
            ^ 0x63;
        tables[value as usize] = forward as u32;
        tables[256 + forward as usize] = value;
    }
    tables
}

/// GF(2^8) 上的乘法, 模 AES 的不可约多项式 x^8 + x^4 + x^3 + x + 1.
fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            product ^= a;
        }
        let high = a & 0x80;
        a <<= 1;
        if high != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    product
}

/// GF(2^8) 上的乘法逆元, 0 的逆元按约定取 0.
fn gf_inverse(value: u8) -> u8 {
    if value == 0 {
        return 0;
    }
    for candidate in 1..=u8::MAX {
        if gf_mul(value, candidate) == 1 {
            return candidate;
        }
    }
    0
}

/// 8 位循环左移, 用于 AES 的仿射变换.
fn rotl8(value: u8, bits: u32) -> u8 {
    value.rotate_left(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sbox_matches_known_values() {
        let tables = sbox_tables();
        assert_eq!(tables[0x00], 0x63);
        assert_eq!(tables[0x01], 0x7c);
        assert_eq!(tables[0x53], 0xed);
        assert_eq!(tables[256 + 0x63], 0x00);
        assert_eq!(tables[256 + 0x7c], 0x01);
        // 逆 S 盒必须是正向 S 盒的置换.
        let mut seen = [false; 256];
        for value in 0..256u32 {
            let forward = tables[value as usize] as usize;
            assert!(!seen[forward], "S 盒在 {value} 处重复");
            seen[forward] = true;
        }
    }

    #[test]
    fn gf_inverse_round_trips() {
        for value in 1..=u8::MAX {
            let inverse = gf_inverse(value);
            assert_eq!(gf_mul(value, inverse), 1, "value = {value}");
        }
        assert_eq!(gf_inverse(0), 0);
    }

    #[test]
    fn param_block_matches_kernel_layout() {
        // 内核的 Params 是 4 个 u32 加 8 项 tail_lengths, 一共 48 字节.
        assert_eq!(std::mem::size_of::<Params>(), 4 * 4 + MAX_WXIDS * 4);
        let words = param_words(2, 3, 7, 9, &[11, 12]);
        assert_eq!(&words[..4], &[7, 9, 2, 3]);
        assert_eq!(words[4], 11);
        assert_eq!(words[5], 12);
        assert_eq!(words[6], 0);
    }
}
