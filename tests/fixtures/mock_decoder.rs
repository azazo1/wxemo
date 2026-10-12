//! 合成 _5 解码接口, 仅供独立进程与超时测试使用.

const PNG: &[u8] = include_bytes!("mock-animation.png");

#[no_mangle]
pub unsafe extern "C" fn wxam_dec_wxam2pic_5(
    input: *const u8,
    input_length: i32,
    output: *mut u8,
    output_length: *mut i32,
    options: *const i32,
) -> i32 {
    if input_length <= 0
        || input.is_null()
        || output.is_null()
        || output_length.is_null()
        || options.is_null()
    {
        return 1;
    }
    if unsafe { *options } != 2 || unsafe { *output_length } < PNG.len() as i32 {
        return 1;
    }
    match unsafe { *input.add(input_length as usize - 1) } {
        1 => return 1,
        2 => std::thread::sleep(std::time::Duration::from_secs(3)),
        _ => {}
    }
    unsafe {
        std::ptr::copy_nonoverlapping(PNG.as_ptr(), output, PNG.len());
        *output_length = PNG.len() as i32;
    }
    0
}
