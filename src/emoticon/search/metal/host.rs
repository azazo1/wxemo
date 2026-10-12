//! Metal compute 后端的主机侧.
//!
//! MSL 源码在运行期由驱动自带的编译器编译 (见 `kernel.metal`), 不依赖构建期的
//! metal 编译器, 因此本机缺少 Metal Toolchain 组件也能跑.
//!
//! 调度上把搜索范围切成固定大小的批次, 每个批次一个 command buffer, 同时保持
//! 若干个批次在途, 用最早那个批次的 `waitUntilCompleted` 推进进度, 这样 GPU
//! 不会因为等待主机而空转.

use std::collections::VecDeque;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use indicatif::ProgressBar;
use metal::foreign_types::{ForeignType, ForeignTypeRef};
use metal::{
    Buffer, CommandBuffer, CommandQueue, CompileOptions, ComputePipelineState, Device,
    MTLResourceOptions, MTLSize,
};
use objc::rc::autoreleasepool;
use objc::runtime::Object;
use tracing::debug;

use crate::emoticon::search::{confirm, Hit, SearchSpec};

/// 内嵌的 MSL 源码. `include_str!` 只是把文本读进来, 不会触发 metal 编译器.
const KERNEL_SOURCE: &str = include_str!("kernel.metal");

/// kernel 入口名.
const KERNEL_NAME: &str = "seed_search";

/// 一个批次覆盖的候选数.
const BATCH: u64 = 1 << 22;

/// 同时在途的 command buffer 数量. 太小会让 GPU 等主机, 太大会浪费提前退出的机会.
const MAX_INFLIGHT: usize = 4;

/// 线程组大小.
const THREADGROUP: u64 = 256;

/// 每批最多记录这么多命中, 超过就只置标志不再写结果.
const MAX_HITS: usize = 64;

/// 与 MSL 里的 `SeedParams` 逐字段对应.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct SeedParams {
    /// 本批第一个候选 seed.
    base: u64,
    /// 每个账号标识覆盖的候选数.
    count: u32,
    /// 账号标识数量.
    wxid_count: u32,
    /// 样本数量.
    sample_count: u32,
    /// 结果缓冲容量.
    max_hits: u32,
}

/// 与 MSL 里的 `WxidEntry` 逐字段对应.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct WxidEntry {
    /// 在账号标识缓冲里的字节偏移.
    offset: u32,
    /// 字节长度.
    length: u32,
}

/// 一个在途批次独占的资源.
struct Slot {
    /// 批次参数.
    params: Buffer,
    /// 命中的 seed.
    hits: Buffer,
    /// 命中数量, kernel 里是 atomic_uint.
    count: Buffer,
}

impl Slot {
    fn new(device: &Device) -> Self {
        let shared = MTLResourceOptions::StorageModeShared;
        Self {
            params: device.new_buffer(std::mem::size_of::<SeedParams>() as u64, shared),
            hits: device.new_buffer((MAX_HITS * std::mem::size_of::<u64>()) as u64, shared),
            count: device.new_buffer(std::mem::size_of::<u32>() as u64, shared),
        }
    }

    /// 清空命中计数.
    fn reset_count(&self) {
        unsafe { std::ptr::write_bytes(self.count.contents() as *mut u32, 0, 1) };
    }

    /// 写入批次参数.
    fn write_params(&self, params: SeedParams) {
        unsafe { std::ptr::write(self.params.contents() as *mut SeedParams, params) };
    }

    /// 读出本批命中的 seed.
    fn take_hits(&self) -> Vec<u64> {
        let count = unsafe { std::ptr::read(self.count.contents() as *const u32) } as usize;
        if count == 0 {
            return Vec::new();
        }
        let count = count.min(MAX_HITS);
        let slice =
            unsafe { std::slice::from_raw_parts(self.hits.contents() as *const u64, count) };
        slice.to_vec()
    }
}

/// 一次 Metal 搜索的全部设备侧资源.
struct Engine {
    queue: CommandQueue,
    pipeline: ComputePipelineState,
    wxid_data: Buffer,
    wxid_table: Buffer,
    sample_prefixes: Buffer,
    stop_flag: Buffer,
    slots: Vec<Slot>,
}

impl Engine {
    fn new(device: &Device, spec: &SearchSpec) -> Result<Self> {
        let shared = MTLResourceOptions::StorageModeShared;

        // 账号标识首尾相接放在一个缓冲里, 用表记录各自的偏移与长度.
        let mut wxid_bytes = Vec::new();
        let mut table = Vec::with_capacity(spec.wxids.len());
        for wxid in &spec.wxids {
            table.push(WxidEntry {
                offset: wxid_bytes.len() as u32,
                length: wxid.len() as u32,
            });
            wxid_bytes.extend_from_slice(wxid);
        }

        // 样本首块: 每个样本 16 字节.
        let mut prefixes = Vec::with_capacity(spec.samples.len() * 16);
        for sample in &spec.samples {
            prefixes.extend_from_slice(&sample.prefix);
        }

        let compiled = Instant::now();
        let library = device
            .new_library_with_source(KERNEL_SOURCE, &CompileOptions::new())
            .map_err(|err| anyhow::anyhow!("编译 MSL 失败: {err}"))?;
        let function = library
            .get_function(KERNEL_NAME, None)
            .map_err(|err| anyhow::anyhow!("找不到 kernel {KERNEL_NAME}: {err}"))?;
        let pipeline = device
            .new_compute_pipeline_state_with_function(&function)
            .map_err(|err| anyhow::anyhow!("创建 compute pipeline 失败: {err}"))?;
        debug!(
            ms = compiled.elapsed().as_secs_f64() * 1000.0,
            source_bytes = KERNEL_SOURCE.len(),
            "MSL 运行期编译完成"
        );

        Ok(Self {
            queue: device.new_command_queue(),
            pipeline,
            wxid_data: device.new_buffer_with_data(
                wxid_bytes.as_ptr() as *const _,
                wxid_bytes.len() as u64,
                shared,
            ),
            wxid_table: device.new_buffer_with_data(
                table.as_ptr() as *const _,
                std::mem::size_of_val(table.as_slice()) as u64,
                shared,
            ),
            sample_prefixes: device.new_buffer_with_data(
                prefixes.as_ptr() as *const _,
                prefixes.len() as u64,
                shared,
            ),
            stop_flag: device.new_buffer(std::mem::size_of::<u32>() as u64, shared),
            slots: (0..MAX_INFLIGHT).map(|_| Slot::new(device)).collect(),
        })
    }

    /// 清空提前退出标志.
    fn reset_stop(&self) {
        unsafe { std::ptr::write_bytes(self.stop_flag.contents() as *mut u32, 0, 1) };
    }

    /// 提交一个批次, 返回可以等待完成的 command buffer.
    fn submit(&self, slot: &Slot, base: u64, count: u64, spec: &SearchSpec) -> CommandBuffer {
        slot.reset_count();
        slot.write_params(SeedParams {
            base,
            count: count as u32,
            wxid_count: spec.wxids.len() as u32,
            sample_count: spec.samples.len() as u32,
            max_hits: MAX_HITS as u32,
        });

        let threads = count * spec.wxids.len() as u64;
        let groups = threads.div_ceil(THREADGROUP);

        // 编码器是 autorelease 对象, 每个批次用一个自动释放池收尾, 避免长时间搜索时堆积.
        autoreleasepool(|| {
            let command = self.queue.new_command_buffer();
            let encoder = command.new_compute_command_encoder();
            encoder.set_compute_pipeline_state(&self.pipeline);
            encoder.set_buffer(0, Some(&self.wxid_data), 0);
            encoder.set_buffer(1, Some(&self.wxid_table), 0);
            encoder.set_buffer(2, Some(&self.sample_prefixes), 0);
            encoder.set_buffer(3, Some(&slot.params), 0);
            encoder.set_buffer(4, Some(&slot.hits), 0);
            encoder.set_buffer(5, Some(&slot.count), 0);
            encoder.set_buffer(6, Some(&self.stop_flag), 0);
            encoder.dispatch_thread_groups(
                MTLSize::new(groups, 1, 1),
                MTLSize::new(THREADGROUP, 1, 1),
            );
            encoder.end_encoding();
            commit_owned(command)
        })
    }
}

/// 把 `new_command_buffer` 返回的 autorelease 对象接管过来并提交.
///
/// `commandBuffer` 返回的引用是 autorelease 的, 直接塞进拥有所有权的包装会在
/// 自动释放池收尾时被提前释放, 所以要先 retain 一次再交给它管理.
fn commit_owned(command: &metal::CommandBufferRef) -> CommandBuffer {
    let raw = command.as_ptr() as *mut Object;
    // SAFETY: command 是有效的 command buffer, objc_retain 返回同一对象的 +1 引用.
    let retained = unsafe { objc::runtime::objc_retain(raw) };
    // SAFETY: retained 是 +1 引用, 交给拥有所有权的包装后由它负责释放一次.
    let owned = unsafe { CommandBuffer::from_ptr(retained as *mut _) };
    owned.commit();
    owned
}

/// 可用的 Metal 设备名, 没有设备时返回 `None`.
pub fn device_name() -> Option<String> {
    Device::system_default().map(|device| device.name().to_string())
}

/// 用 Metal 后端在 `spec` 范围内搜索, 找到即返回.
pub fn run(spec: &SearchSpec, bar: &ProgressBar) -> Result<Option<Hit>> {
    if spec.wxids.is_empty() {
        bail!("没有可用的账号标识");
    }
    if spec.samples.is_empty() {
        bail!("没有可用的判定样本");
    }

    // kernel 只处理一个 MD5 块, 消息长度必须装得下.
    let max_digits = decimal_digits(spec.end.saturating_sub(1));
    for wxid in &spec.wxids {
        if max_digits + wxid.len() + SUFFIX_LEN >= 56 {
            bail!(
                "账号标识过长, 消息超过单个 MD5 块: 位数 {max_digits} + 账号标识 {} 字节",
                wxid.len()
            );
        }
    }

    let device = Device::system_default().context("没有可用的 Metal 设备")?;
    let engine = Engine::new(&device, spec)?;

    let max_threads = engine.pipeline.max_total_threads_per_threadgroup();
    debug!(
        device = %device.name(),
        max_total_threads_per_threadgroup = max_threads,
        batch = BATCH,
        inflight = MAX_INFLIGHT,
        "Metal 搜索初始化完成"
    );

    engine.reset_stop();

    let total = spec.end - spec.start;
    let mut inflight: VecDeque<(u64, usize, CommandBuffer)> = VecDeque::new();
    let mut next_slot = 0usize;
    let mut submitted = spec.start;
    let mut found: Option<u64> = None;

    while submitted < spec.end {
        let count = BATCH.min(spec.end - submitted);
        let slot = next_slot;
        let command = engine.submit(&engine.slots[slot], submitted, count, spec);
        inflight.push_back((submitted + count, slot, command));
        submitted += count;

        if inflight.len() >= MAX_INFLIGHT {
            if let Some(seed) = reap(&engine, &mut inflight, bar, spec.start) {
                found = Some(seed);
                break;
            }
        }
        next_slot = (next_slot + 1) % MAX_INFLIGHT;
    }

    // 排空在途批次. 命中后仍然等待, 保证缓冲不会被复用中的批次改写.
    while !inflight.is_empty() {
        if let Some(seed) = reap(&engine, &mut inflight, bar, spec.start) {
            found = found.or(Some(seed));
        }
    }

    bar.set_position(total);

    let Some(seed) = found else {
        return Ok(None);
    };

    // GPU 只做首块魔数筛选, 命中后回到与 CPU 完全相同的路径做复核.
    for index in 0..spec.wxids.len() {
        if let Some(hit) = confirm(spec, seed, index) {
            return Ok(Some(hit));
        }
    }
    debug!(seed, "GPU 报出的候选没有通过复核");
    Ok(None)
}

/// 等待最早的批次完成并检查它的结果.
fn reap(
    engine: &Engine,
    inflight: &mut VecDeque<(u64, usize, CommandBuffer)>,
    bar: &ProgressBar,
    start: u64,
) -> Option<u64> {
    let (end, slot, command) = inflight.pop_front()?;
    command.wait_until_completed();
    bar.set_position(end - start);

    let hits = engine.slots[slot].take_hits();
    if hits.is_empty() {
        return None;
    }
    hits.into_iter().min()
}

/// 十进制位数, 0 算一位.
fn decimal_digits(value: u64) -> usize {
    let mut digits = 1;
    let mut rest = value;
    while rest >= 10 {
        rest /= 10;
        digits += 1;
    }
    digits
}

/// 消息末尾后缀的长度.
const SUFFIX_LEN: usize = 8;

/// 测试探针: 用 `derive_keys` kernel 批量派生密钥, 供与 Rust 侧逐字节比对.
///
/// 只编译进测试, 不进入正式二进制.
#[cfg(test)]
pub(crate) fn derive_keys_probe(base: u64, count: u64, wxid: &[u8]) -> Result<Vec<[u8; 16]>> {
    let device = Device::system_default().context("没有可用的 Metal 设备")?;
    let shared = MTLResourceOptions::StorageModeShared;

    let library = device
        .new_library_with_source(KERNEL_SOURCE, &CompileOptions::new())
        .map_err(|err| anyhow::anyhow!("编译 MSL 失败: {err}"))?;
    let function = library
        .get_function("derive_keys", None)
        .map_err(|err| anyhow::anyhow!("找不到 kernel derive_keys: {err}"))?;
    let pipeline = device
        .new_compute_pipeline_state_with_function(&function)
        .map_err(|err| anyhow::anyhow!("创建 compute pipeline 失败: {err}"))?;

    let table = [WxidEntry {
        offset: 0,
        length: wxid.len() as u32,
    }];
    let wxid_buffer =
        device.new_buffer_with_data(wxid.as_ptr() as *const _, wxid.len() as u64, shared);
    let table_buffer = device.new_buffer_with_data(
        table.as_ptr() as *const _,
        std::mem::size_of_val(&table) as u64,
        shared,
    );
    let params_buffer = device.new_buffer(std::mem::size_of::<SeedParams>() as u64, shared);
    let keys_buffer = device.new_buffer(16 * count, shared);

    let params = SeedParams {
        base,
        count: count as u32,
        wxid_count: 1,
        sample_count: 1,
        max_hits: 1,
    };
    unsafe { std::ptr::write(params_buffer.contents() as *mut SeedParams, params) };

    let queue = device.new_command_queue();
    autoreleasepool(|| {
        let command = queue.new_command_buffer();
        let encoder = command.new_compute_command_encoder();
        encoder.set_compute_pipeline_state(&pipeline);
        encoder.set_buffer(0, Some(&wxid_buffer), 0);
        encoder.set_buffer(1, Some(&table_buffer), 0);
        encoder.set_buffer(3, Some(&params_buffer), 0);
        encoder.set_buffer(4, Some(&keys_buffer), 0);
        encoder.dispatch_thread_groups(
            MTLSize::new(count.div_ceil(THREADGROUP), 1, 1),
            MTLSize::new(THREADGROUP, 1, 1),
        );
        encoder.end_encoding();
        let owned = commit_owned(command);
        owned.wait_until_completed();
    });

    let raw = unsafe {
        std::slice::from_raw_parts(keys_buffer.contents() as *const u8, (16 * count) as usize)
    };
    Ok(raw
        .as_chunks::<16>()
        .0
        .iter()
        .map(|chunk| {
            let mut key = [0u8; 16];
            key.copy_from_slice(chunk);
            key
        })
        .collect())
}
