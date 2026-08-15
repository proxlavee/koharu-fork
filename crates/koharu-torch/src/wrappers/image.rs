use super::tensor::Tensor;
use super::utils::path_to_cstring;
use crate::TchError;
use libc::c_int;
use std::path::Path;

/// On success returns a tensor of shape [width, height, channels].
pub fn load_hwc<T: AsRef<Path>>(path: T) -> Result<Tensor, TchError> {
    let path = path_to_cstring(path)?;
    let c_tensor = unsafe_torch_err!(koharu_torch_sys::at_load_image(path.as_ptr()));
    Ok(Tensor { c_tensor })
}

/// On success returns a tensor of shape [width, height, channels].
pub fn load_hwc_from_mem(data: &[u8]) -> Result<Tensor, TchError> {
    let c_tensor = unsafe_torch_err!(koharu_torch_sys::at_load_image_from_memory(
        data.as_ptr(),
        data.len()
    ));
    Ok(Tensor { c_tensor })
}

/// Expects a tensor of shape [width, height, channels].
pub fn save_hwc<T: AsRef<Path>>(t: &Tensor, path: T) -> Result<(), TchError> {
    let path = path_to_cstring(path)?;
    let _ = unsafe_torch_err!(koharu_torch_sys::at_save_image(t.c_tensor, path.as_ptr()));
    Ok(())
}

/// Expects a tensor of shape [width, height, channels].
/// On success returns a tensor of shape [width, height, channels].
pub fn resize_hwc(t: &Tensor, out_w: i64, out_h: i64) -> Result<Tensor, TchError> {
    let out_w = image_dimension(out_w, "width")?;
    let out_h = image_dimension(out_h, "height")?;
    let c_tensor = unsafe_torch_err!(koharu_torch_sys::at_resize_image(t.c_tensor, out_w, out_h));
    Ok(Tensor { c_tensor })
}

fn image_dimension(value: i64, name: &str) -> Result<c_int, TchError> {
    if value <= 0 {
        return Err(TchError::Shape(format!(
            "image {name} must be greater than zero, got {value}"
        )));
    }
    c_int::try_from(value).map_err(|_| {
        TchError::Shape(format!(
            "image {name} exceeds the native image limit, got {value}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_image_dimensions_reject_invalid_values_before_ffi() {
        assert!(image_dimension(1, "width").is_ok());
        assert!(image_dimension(0, "width").is_err());
        assert!(image_dimension(-1, "width").is_err());
        assert!(image_dimension(i64::from(c_int::MAX) + 1, "width").is_err());
    }
}
