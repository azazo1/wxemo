//! CUDA 驱动 API 的最小绑定与执行.
//!
//! 刻意不使用 cudarc: 它的运行期编译走 nvrtc, 而 WSL 里既没有 nvrtc 也没有
//! libnvJitLink, 只有运行期的 libcuda.so. 这里直接用 libloading 调驱动 API,
//! 镜像用构建期产出的 cubin 通过 cuModuleLoadData 加载, 不依赖任何工具链.

use std::ffi::{c_char, c_void, CStr};
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _, Result};
use libloading::Library;
use tracing::debug;

/// 构建期由 build.rs 生成的 cubin 镜像.
const CUBIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wxemo_cuda_kernel.cubin"));

/// 驱动 API 的返回码类型.
type CuResult = i32;

/// `CUDA_SUCCESS`.
const CUDA_SUCCESS: CuResult = 0;

/// CUDA 上下文句柄.
type CuContext = *mut c_void;
/// CUDA 模块句柄.
type CuModule = *mut c_void;
/// CUDA 内核句柄.
type CuFunction = *mut c_void;
/// CUDA 设备句柄, 就是一个整数编号.
type CuDevice = i32;
/// 设备指针.
type CuDeviceptr = u64;

/// `CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR`.
const ATTR_CC_MAJOR: i32 = 75;
/// `CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR`.
const ATTR_CC_MINOR: i32 = 76;
/// `CU_DEVICE_ATTRIBUTE_MULTIPROCESSOR_COUNT`.
const ATTR_MULTIPROCESSOR_COUNT: i32 = 16;

/// 内核入口名字, 必须与 kernel.cu 里的 `wxemo_search_kernel` 一致.
const KERNEL: &str = "wxemo_search_kernel";

/// 每个线程块的线程数.
const BLOCK: u32 = 256;

/// 一个批次覆盖的候选数.
///
/// 批次既是进度条的推进单位, 也是提前退出的最小粒度. 取 2^22 时在 RTX 4060 上
/// 单批只要几毫秒, 进度条足够顺滑, 而多出来的内核启动开销可以忽略.
const BATCH: u64 = 1 << 22;

/// 常量内存里数组的容量, 必须与 kernel.cu 的宏一致.
const WXID_COUNT_MAX: usize = 4;
const WXID_MAX: usize = 32;
const SAMPLE_MAX: usize = 16;

/// 设备信息, 用于日志与可用性检查.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// 设备型号.
    pub name: String,
    /// compute capability 主版本.
    pub major: i32,
    /// compute capability 次版本.
    pub minor: i32,
    /// 多处理器数量.
    pub multiprocessors: i32,
}

// ------------------------------------------------------------ 驱动符号表

type CuInit = unsafe extern "C" fn(u32) -> CuResult;
type CuDeviceGet = unsafe extern "C" fn(*mut CuDevice, i32) -> CuResult;
type CuDeviceGetName = unsafe extern "C" fn(*mut c_char, i32, CuDevice) -> CuResult;
type CuDeviceGetAttribute = unsafe extern "C" fn(*mut i32, i32, CuDevice) -> CuResult;
type CuCtxCreate = unsafe extern "C" fn(*mut CuContext, u32, CuDevice) -> CuResult;
type CuCtxDestroy = unsafe extern "C" fn(CuContext) -> CuResult;
type CuCtxSynchronize = unsafe extern "C" fn() -> CuResult;
type CuModuleLoadData = unsafe extern "C" fn(*mut CuModule, *const c_void) -> CuResult;
type CuModuleGetFunction =
    unsafe extern "C" fn(*mut CuFunction, CuModule, *const c_char) -> CuResult;
type CuModuleGetGlobal =
    unsafe extern "C" fn(*mut CuDeviceptr, *mut usize, CuModule, *const c_char) -> CuResult;
type CuMemAlloc = unsafe extern "C" fn(*mut CuDeviceptr, usize) -> CuResult;
type CuMemFree = unsafe extern "C" fn(CuDeviceptr) -> CuResult;
type CuMemcpyHtoD = unsafe extern "C" fn(CuDeviceptr, *const c_void, usize) -> CuResult;
type CuMemcpyDtoH = unsafe extern "C" fn(*mut c_void, CuDeviceptr, usize) -> CuResult;
type CuLaunchKernel = unsafe extern "C" fn(
    CuFunction,
    u32,
    u32,
    u32,
    u32,
    u32,
    u32,
    u32,
    *mut c_void,
    *mut *mut c_void,
    *mut *mut c_void,
) -> CuResult;
type CuGetError = unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult;

/// 驱动函数指针表.
///
/// `_library` 放在最后, 保证析构顺序上动态库晚于函数指针释放.
struct Driver {
    cu_init: CuInit,
    cu_device_get: CuDeviceGet,
    cu_device_get_name: CuDeviceGetName,
    cu_device_get_attribute: CuDeviceGetAttribute,
    cu_ctx_create: CuCtxCreate,
    cu_ctx_destroy: CuCtxDestroy,
    cu_ctx_synchronize: CuCtxSynchronize,
    cu_module_load_data: CuModuleLoadData,
    cu_module_get_function: CuModuleGetFunction,
    cu_module_get_global: CuModuleGetGlobal,
    cu_mem_alloc: CuMemAlloc,
    cu_mem_free: CuMemFree,
    cu_memcpy_h_to_d: CuMemcpyHtoD,
    cu_memcpy_d_to_h: CuMemcpyDtoH,
    cu_launch_kernel: CuLaunchKernel,
    cu_get_error_name: CuGetError,
    cu_get_error_string: CuGetError,
    _library: Library,
}

/// 取一个驱动符号, 依次尝试给出的候选名字.
///
/// 64 位平台上 CUDA 头文件把 `cuCtxCreate` 这类名字宏映射到 `_v2` 版本,
/// 动态库通常导出 `_v2`, 但个别版本只留了旧名, 所以两个都要试.
unsafe fn fetch<T: Copy>(library: &Library, names: &[&str]) -> Result<T> {
    for name in names {
        let mut buffer = Vec::with_capacity(name.len() + 1);
        buffer.extend_from_slice(name.as_bytes());
        buffer.push(0);
        if let Ok(symbol) = library.get::<T>(&buffer) {
            return Ok(*symbol);
        }
    }
    bail!("CUDA 驱动缺少符号 {}", names.join(" 或 "))
}

impl Driver {
    fn load() -> Result<Self> {
        let library = open_library()?;
        unsafe {
            Ok(Self {
                cu_init: fetch(&library, &["cuInit"])?,
                cu_device_get: fetch(&library, &["cuDeviceGet"])?,
                cu_device_get_name: fetch(&library, &["cuDeviceGetName"])?,
                cu_device_get_attribute: fetch(&library, &["cuDeviceGetAttribute"])?,
                cu_ctx_create: fetch(&library, &["cuCtxCreate_v2", "cuCtxCreate"])?,
                cu_ctx_destroy: fetch(&library, &["cuCtxDestroy_v2", "cuCtxDestroy"])?,
                cu_ctx_synchronize: fetch(&library, &["cuCtxSynchronize"])?,
                cu_module_load_data: fetch(&library, &["cuModuleLoadData", "cuModuleLoadDataEx"])?,
                cu_module_get_function: fetch(&library, &["cuModuleGetFunction"])?,
                cu_module_get_global: fetch(
                    &library,
                    &["cuModuleGetGlobal_v2", "cuModuleGetGlobal"],
                )?,
                cu_mem_alloc: fetch(&library, &["cuMemAlloc_v2", "cuMemAlloc"])?,
                cu_mem_free: fetch(&library, &["cuMemFree_v2", "cuMemFree"])?,
                cu_memcpy_h_to_d: fetch(&library, &["cuMemcpyHtoD_v2", "cuMemcpyHtoD"])?,
                cu_memcpy_d_to_h: fetch(&library, &["cuMemcpyDtoH_v2", "cuMemcpyDtoH"])?,
                cu_launch_kernel: fetch(&library, &["cuLaunchKernel"])?,
                cu_get_error_name: fetch(&library, &["cuGetErrorName"])?,
                cu_get_error_string: fetch(&library, &["cuGetErrorString"])?,
                _library: library,
            })
        }
    }

    /// 把返回码翻译成人能看懂的文本.
    fn describe(&self, code: CuResult) -> String {
        unsafe {
            let mut name: *const c_char = std::ptr::null();
            let mut text: *const c_char = std::ptr::null();
            let _ = (self.cu_get_error_name)(code, &mut name);
            let _ = (self.cu_get_error_string)(code, &mut text);
            let name = if name.is_null() {
                String::new()
            } else {
                CStr::from_ptr(name).to_string_lossy().to_string()
            };
            let text = if text.is_null() {
                String::new()
            } else {
                CStr::from_ptr(text).to_string_lossy().to_string()
            };
            match (name.is_empty(), text.is_empty()) {
                (false, false) => format!("{name} / {text}"),
                (false, true) => name,
                (true, false) => text,
                (true, true) => format!("返回码 {code}"),
            }
        }
    }

    /// 返回码不是 `CUDA_SUCCESS` 时转成带名字和描述的 `anyhow` 错误.
    fn check(&self, what: &str, code: CuResult) -> Result<()> {
        if code == CUDA_SUCCESS {
            return Ok(());
        }
        Err(anyhow!("{what} 失败: {}", self.describe(code)))
    }
}

/// 打开 CUDA 驱动动态库.
///
/// WSL 只提供 `/usr/lib/wsl/lib/libcuda.so` 这一个入口, 常规 Linux 发行版
/// 通常是 `libcuda.so.1`, Windows 是 `nvcuda.dll`.
fn open_library() -> Result<Library> {
    let candidates: &[&str] = if cfg!(windows) {
        &["nvcuda.dll", "C:/Windows/System32/nvcuda.dll"]
    } else {
        &[
            "/usr/lib/wsl/lib/libcuda.so",
            "/usr/lib/wsl/lib/libcuda.so.1",
            "libcuda.so.1",
            "libcuda.so",
        ]
    };

    let mut errors = Vec::new();
    for candidate in candidates {
        if candidate.starts_with('/') && !Path::new(candidate).exists() {
            continue;
        }
        // RTLD_GLOBAL 由 libloading 默认提供, WSL 的 libcuda.so 还要加载同目录的 libnvidia-*.
        match unsafe { Library::new(candidate) } {
            Ok(library) => {
                debug!(library = candidate, "已加载 CUDA 驱动");
                return Ok(library);
            }
            Err(error) => errors.push(format!("{candidate}: {error}")),
        }
    }
    bail!("无法加载 CUDA 驱动动态库: {}", errors.join("; "))
}

/// 读取设备型号与计算能力.
fn read_device_info(driver: &Driver, device: CuDevice) -> Result<DeviceInfo> {
    let mut raw = [0 as c_char; 256];
    let mut major = 0;
    let mut minor = 0;
    let mut multiprocessors = 0;
    unsafe {
        driver.check(
            "cuDeviceGetName",
            (driver.cu_device_get_name)(raw.as_mut_ptr(), raw.len() as i32, device),
        )?;
        driver.check(
            "cuDeviceGetAttribute",
            (driver.cu_device_get_attribute)(&mut major, ATTR_CC_MAJOR, device),
        )?;
        driver.check(
            "cuDeviceGetAttribute",
            (driver.cu_device_get_attribute)(&mut minor, ATTR_CC_MINOR, device),
        )?;
        driver.check(
            "cuDeviceGetAttribute",
            (driver.cu_device_get_attribute)(
                &mut multiprocessors,
                ATTR_MULTIPROCESSOR_COUNT,
                device,
            ),
        )?;
    }
    Ok(DeviceInfo {
        name: unsafe { CStr::from_ptr(raw.as_ptr()) }
            .to_string_lossy()
            .to_string(),
        major,
        minor,
        multiprocessors,
    })
}

/// 探测设备: 初始化驱动并读一次设备属性.
///
/// 刻意不建上下文: 查询设备属性不需要上下文, 而建/销毁上下文的代价有几十毫秒,
/// 放在 `--backend auto` 的可用性判断里纯属浪费. 真正的上下文由 `Context::new` 建.
pub fn probe() -> Result<DeviceInfo> {
    let driver = Driver::load()?;
    unsafe {
        driver.check("cuInit", (driver.cu_init)(0))?;
        let mut device: CuDevice = 0;
        let code = (driver.cu_device_get)(&mut device, 0);
        if code != CUDA_SUCCESS {
            bail!("没有可用的 CUDA 设备: {}", driver.describe(code));
        }
        read_device_info(&driver, device)
    }
}

// ------------------------------------------------------------ 上下文

/// 一个已经就绪的 CUDA 执行环境.
pub struct Context {
    driver: Arc<Driver>,
    context: CuContext,
    module: CuModule,
    function: CuFunction,
    seed_buffer: CuDeviceptr,
    wxid_buffer: CuDeviceptr,
    flag_buffer: CuDeviceptr,
}

impl Context {
    /// 初始化驱动, 选设备, 建上下文, 加载镜像并申请输出缓冲.
    pub fn new() -> Result<Self> {
        let driver = Arc::new(Driver::load()?);
        unsafe {
            driver.check("cuInit", (driver.cu_init)(0))?;

            let mut device: CuDevice = 0;
            let code = (driver.cu_device_get)(&mut device, 0);
            if code != CUDA_SUCCESS {
                bail!("没有可用的 CUDA 设备: {}", driver.describe(code));
            }
            let info = read_device_info(&driver, device)?;
            debug!(
                device = %info.name,
                compute_capability = %format!("{}.{}", info.major, info.minor),
                multiprocessors = info.multiprocessors,
                "已选择 CUDA 设备"
            );

            let mut context: CuContext = std::ptr::null_mut();
            driver.check(
                "cuCtxCreate",
                (driver.cu_ctx_create)(&mut context, 0, device),
            )?;

            // cubin 要按 8 字节对齐地交给驱动, include_bytes! 不保证对齐,
            // 因此先复制到 u64 缓冲区再取指针.
            let words = CUBIN.len().div_ceil(8);
            let mut aligned = vec![0u64; words];
            std::ptr::copy_nonoverlapping(
                CUBIN.as_ptr(),
                aligned.as_mut_ptr().cast::<u8>(),
                CUBIN.len(),
            );

            let mut module: CuModule = std::ptr::null_mut();
            let code = (driver.cu_module_load_data)(&mut module, aligned.as_ptr().cast::<c_void>());
            if code != CUDA_SUCCESS {
                let _ = (driver.cu_ctx_destroy)(context);
                bail!(
                    "加载 CUDA 镜像失败 (cubin {} 字节): {}",
                    CUBIN.len(),
                    driver.describe(code)
                );
            }

            let mut function: CuFunction = std::ptr::null_mut();
            let name = std::ffi::CString::new(KERNEL).expect("内核名不含 NUL");
            driver.check(
                "cuModuleGetFunction",
                (driver.cu_module_get_function)(&mut function, module, name.as_ptr()),
            )?;

            let mut seed_buffer = 0;
            let mut wxid_buffer = 0;
            let mut flag_buffer = 0;
            driver.check("cuMemAlloc", (driver.cu_mem_alloc)(&mut seed_buffer, 8))?;
            driver.check("cuMemAlloc", (driver.cu_mem_alloc)(&mut wxid_buffer, 4))?;
            driver.check("cuMemAlloc", (driver.cu_mem_alloc)(&mut flag_buffer, 4))?;

            Ok(Self {
                driver,
                context,
                module,
                function,
                seed_buffer,
                wxid_buffer,
                flag_buffer,
            })
        }
    }

    /// 写入常量内存里的搜索参数.
    pub fn upload(&self, wxids: &[Vec<u8>], prefixes: &[[u8; 16]]) -> Result<()> {
        if wxids.is_empty() || wxids.len() > WXID_COUNT_MAX {
            bail!("wxid 数量必须在 1..={WXID_COUNT_MAX}, 实际 {}", wxids.len());
        }
        if prefixes.is_empty() || prefixes.len() > SAMPLE_MAX {
            bail!("样本数量必须在 1..={SAMPLE_MAX}, 实际 {}", prefixes.len());
        }
        for wxid in wxids {
            if wxid.is_empty() || wxid.len() > WXID_MAX {
                bail!("wxid 长度必须在 1..={WXID_MAX}, 实际 {}", wxid.len());
            }
        }

        let mut padded = vec![0u8; WXID_COUNT_MAX * WXID_MAX];
        let mut lengths = [0u32; WXID_COUNT_MAX];
        for (index, wxid) in wxids.iter().enumerate() {
            padded[index * WXID_MAX..index * WXID_MAX + wxid.len()].copy_from_slice(wxid);
            lengths[index] = wxid.len() as u32;
        }
        let mut prefixes_padded = vec![0u8; SAMPLE_MAX * 16];
        for (index, prefix) in prefixes.iter().enumerate() {
            prefixes_padded[index * 16..index * 16 + 16].copy_from_slice(prefix);
        }

        self.write_global("g_wxid", &padded)?;
        self.write_global("g_wxid_len", as_bytes(&lengths))?;
        let count = wxids.len() as u32;
        self.write_global("g_wxid_count", as_bytes(&count))?;
        let samples = prefixes.len() as u32;
        self.write_global("g_sample_count", as_bytes(&samples))?;
        self.write_global("g_prefix", &prefixes_padded)?;
        Ok(())
    }

    /// 跑一个批次, 返回是否已经在批内命中.
    pub fn launch(&self, start: u64, count: u64, limit: u64) -> Result<bool> {
        self.clear_outputs()?;
        let mut batch_start = start;
        let mut batch_count = count;
        let mut batch_limit = limit;
        let mut out_seed = self.seed_buffer;
        let mut out_wxid = self.wxid_buffer;
        let mut out_flag = self.flag_buffer;

        let mut arguments: [*mut c_void; 6] = [
            (&mut batch_start as *mut u64).cast::<c_void>(),
            (&mut batch_count as *mut u64).cast::<c_void>(),
            (&mut batch_limit as *mut u64).cast::<c_void>(),
            (&mut out_seed as *mut CuDeviceptr).cast::<c_void>(),
            (&mut out_wxid as *mut CuDeviceptr).cast::<c_void>(),
            (&mut out_flag as *mut CuDeviceptr).cast::<c_void>(),
        ];

        let grid = count.div_ceil(u64::from(BLOCK)).min(u64::from(u32::MAX)) as u32;
        unsafe {
            self.driver.check(
                "cuLaunchKernel",
                (self.driver.cu_launch_kernel)(
                    self.function,
                    grid,
                    1,
                    1,
                    BLOCK,
                    1,
                    1,
                    0,
                    std::ptr::null_mut(),
                    arguments.as_mut_ptr(),
                    std::ptr::null_mut(),
                ),
            )?;
            self.driver
                .check("cuCtxSynchronize", (self.driver.cu_ctx_synchronize)())?;
        }

        Ok(self.read_u32(self.flag_buffer)? != 0)
    }

    /// 读回命中结果.
    pub fn hit(&self) -> Result<(u64, u32)> {
        let mut seed = 0u64;
        let mut wxid = 0u32;
        unsafe {
            self.driver.check(
                "cuMemcpyDtoH",
                (self.driver.cu_memcpy_d_to_h)(
                    (&mut seed as *mut u64).cast::<c_void>(),
                    self.seed_buffer,
                    8,
                ),
            )?;
            self.driver.check(
                "cuMemcpyDtoH",
                (self.driver.cu_memcpy_d_to_h)(
                    (&mut wxid as *mut u32).cast::<c_void>(),
                    self.wxid_buffer,
                    4,
                ),
            )?;
        }
        Ok((seed, wxid))
    }

    /// 清空命中标志与命中 seed, 让每个批次从干净状态开始.
    fn clear_outputs(&self) -> Result<()> {
        let zeros = [0u8; 8];
        unsafe {
            self.driver.check(
                "cuMemcpyHtoD",
                (self.driver.cu_memcpy_h_to_d)(
                    self.seed_buffer,
                    zeros.as_ptr().cast::<c_void>(),
                    8,
                ),
            )?;
            self.driver.check(
                "cuMemcpyHtoD",
                (self.driver.cu_memcpy_h_to_d)(
                    self.flag_buffer,
                    zeros.as_ptr().cast::<c_void>(),
                    4,
                ),
            )?;
        }
        Ok(())
    }

    /// 读取一个 32 位输出缓冲.
    fn read_u32(&self, pointer: CuDeviceptr) -> Result<u32> {
        let mut value = 0u32;
        unsafe {
            self.driver.check(
                "cuMemcpyDtoH",
                (self.driver.cu_memcpy_d_to_h)(
                    (&mut value as *mut u32).cast::<c_void>(),
                    pointer,
                    4,
                ),
            )?;
        }
        Ok(value)
    }

    /// 往一个 `__constant__` 变量写数据.
    ///
    /// 只接受字节切片: 内核侧的常量都是数组, 用泛型加 `size_of::<T>()` 很容易
    /// 在 `&Vec<u8>` 上悄悄只传 24 字节的胖指针头, 这里从类型上堵掉.
    fn write_global(&self, name: &str, value: &[u8]) -> Result<()> {
        let symbol = std::ffi::CString::new(name).expect("常量名不含 NUL");
        let mut address: CuDeviceptr = 0;
        let mut size: usize = 0;
        unsafe {
            self.driver
                .check(
                    "cuModuleGetGlobal",
                    (self.driver.cu_module_get_global)(
                        &mut address,
                        &mut size,
                        self.module,
                        symbol.as_ptr(),
                    ),
                )
                .with_context(|| format!("查找常量内存 {name} 失败"))?;
            if size < value.len() {
                bail!(
                    "常量内存 {name} 只有 {size} 字节, 需要 {} 字节",
                    value.len()
                );
            }
            self.driver.check(
                "cuMemcpyHtoD",
                (self.driver.cu_memcpy_h_to_d)(
                    address,
                    value.as_ptr().cast::<c_void>(),
                    value.len(),
                ),
            )?;
        }
        Ok(())
    }
}

/// 把 `Pod` 值按字节看.
fn as_bytes<T: Copy>(value: &T) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts((value as *const T).cast::<u8>(), std::mem::size_of::<T>())
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.driver.cu_mem_free)(self.seed_buffer);
            let _ = (self.driver.cu_mem_free)(self.wxid_buffer);
            let _ = (self.driver.cu_mem_free)(self.flag_buffer);
            let _ = (self.driver.cu_ctx_destroy)(self.context);
        }
    }
}

/// 一个批次覆盖的候选数, 供上层日志使用.
pub const fn batch_size() -> u64 {
    BATCH
}
