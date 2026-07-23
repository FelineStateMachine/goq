use std::path::Path;

use anyhow::Result;

use crate::config::VaapiRateControl;

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
    use std::mem::MaybeUninit;
    use std::ptr;

    use anyhow::{Context, Result, ensure};
    use libloading::{Library, Symbol};

    use super::{Path, VaapiRateControl};

    const G_PARAM_READABLE: u32 = 1 << 0;
    const G_PARAM_WRITABLE: u32 = 1 << 1;
    const G_TYPE_ENUM: usize = 12 << 2;

    #[repr(C)]
    struct GTypeInstance {
        g_class: *mut c_void,
    }

    #[repr(C)]
    struct GParamSpec {
        g_type_instance: GTypeInstance,
        name: *const c_char,
        flags: u32,
        value_type: usize,
        owner_type: usize,
    }

    #[repr(C)]
    union GValueData {
        v_int: c_int,
        v_uint: c_uint,
        v_long: isize,
        v_ulong: usize,
        v_int64: i64,
        v_uint64: u64,
        v_float: f32,
        v_double: f64,
        v_pointer: *mut c_void,
    }

    #[repr(C)]
    struct GValue {
        g_type: usize,
        data: [GValueData; 2],
    }

    #[repr(C)]
    struct GTypeClass {
        g_type: usize,
    }

    #[repr(C)]
    struct GEnumValue {
        value: c_int,
        value_name: *const c_char,
        value_nick: *const c_char,
    }

    #[repr(C)]
    struct GEnumClass {
        g_type_class: GTypeClass,
        minimum: c_int,
        maximum: c_int,
        n_values: c_uint,
        values: *mut GEnumValue,
    }

    #[repr(C)]
    struct GError {
        domain: c_uint,
        code: c_int,
        message: *mut c_char,
    }

    type GstInitCheck =
        unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char, *mut *mut GError) -> c_int;
    type GstElementFactoryMake = unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_void;
    type GstObjectUnref = unsafe extern "C" fn(*mut c_void);
    type GObjectClassFindProperty =
        unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut GParamSpec;
    type GObjectGetProperty = unsafe extern "C" fn(*mut c_void, *const c_char, *mut GValue);
    type GValueInit = unsafe extern "C" fn(*mut GValue, usize) -> *mut GValue;
    type GValueGetString = unsafe extern "C" fn(*const GValue) -> *const c_char;
    type GValueUnset = unsafe extern "C" fn(*mut GValue);
    type GTypeFundamental = unsafe extern "C" fn(usize) -> usize;
    type GTypeClassRef = unsafe extern "C" fn(usize) -> *mut c_void;
    type GTypeClassUnref = unsafe extern "C" fn(*mut c_void);
    type GErrorFree = unsafe extern "C" fn(*mut GError);

    struct GstreamerApi<'a> {
        _gstreamer: Library,
        _gobject: Library,
        _glib: Library,
        gst_init_check: Symbol<'a, GstInitCheck>,
        gst_element_factory_make: Symbol<'a, GstElementFactoryMake>,
        gst_object_unref: Symbol<'a, GstObjectUnref>,
        g_object_class_find_property: Symbol<'a, GObjectClassFindProperty>,
        g_object_get_property: Symbol<'a, GObjectGetProperty>,
        g_value_init: Symbol<'a, GValueInit>,
        g_value_get_string: Symbol<'a, GValueGetString>,
        g_value_unset: Symbol<'a, GValueUnset>,
        g_type_fundamental: Symbol<'a, GTypeFundamental>,
        g_type_class_ref: Symbol<'a, GTypeClassRef>,
        g_type_class_unref: Symbol<'a, GTypeClassUnref>,
        g_error_free: Symbol<'a, GErrorFree>,
    }

    impl GstreamerApi<'_> {
        unsafe fn load() -> Result<Self> {
            let gstreamer = unsafe { Library::new("libgstreamer-1.0.so.0") }
                .context("loading libgstreamer-1.0.so.0")?;
            let gobject = unsafe { Library::new("libgobject-2.0.so.0") }
                .context("loading libgobject-2.0.so.0")?;
            let glib =
                unsafe { Library::new("libglib-2.0.so.0") }.context("loading libglib-2.0.so.0")?;

            // SAFETY: every symbol uses the public C ABI declared by the matching
            // versioned runtime SONAME. The libraries remain owned by this value
            // for at least as long as all function symbols below.
            unsafe {
                Ok(Self {
                    gst_init_check: gstreamer.get(b"gst_init_check\0")?,
                    gst_element_factory_make: gstreamer.get(b"gst_element_factory_make\0")?,
                    gst_object_unref: gstreamer.get(b"gst_object_unref\0")?,
                    g_object_class_find_property: gobject.get(b"g_object_class_find_property\0")?,
                    g_object_get_property: gobject.get(b"g_object_get_property\0")?,
                    g_value_init: gobject.get(b"g_value_init\0")?,
                    g_value_get_string: gobject.get(b"g_value_get_string\0")?,
                    g_value_unset: gobject.get(b"g_value_unset\0")?,
                    g_type_fundamental: gobject.get(b"g_type_fundamental\0")?,
                    g_type_class_ref: gobject.get(b"g_type_class_ref\0")?,
                    g_type_class_unref: gobject.get(b"g_type_class_unref\0")?,
                    g_error_free: glib.get(b"g_error_free\0")?,
                    _gstreamer: gstreamer,
                    _gobject: gobject,
                    _glib: glib,
                })
            }
        }

        unsafe fn property(&self, object: *mut c_void, name: &CStr) -> Result<&GParamSpec> {
            // SAFETY: GstElement begins with a GTypeInstance, whose class pointer
            // is valid while the element is alive. GObject returns a class-owned
            // GParamSpec that remains valid for the lifetime of that class.
            let class = unsafe { (*(object.cast::<GTypeInstance>())).g_class };
            let property = unsafe { (self.g_object_class_find_property)(class, name.as_ptr()) };
            ensure!(
                !property.is_null(),
                "encoder has no {} property",
                name.to_string_lossy()
            );
            Ok(unsafe { &*property })
        }

        unsafe fn string_property(
            &self,
            object: *mut c_void,
            name: &CStr,
            property: &GParamSpec,
        ) -> Result<String> {
            let mut value = MaybeUninit::<GValue>::zeroed();
            let value = value.as_mut_ptr();
            unsafe {
                (self.g_value_init)(value, property.value_type);
                (self.g_object_get_property)(object, name.as_ptr(), value);
            }
            let observed = unsafe { (self.g_value_get_string)(value) };
            let result = ensure_non_null_string(observed, name);
            unsafe { (self.g_value_unset)(value) };
            result
        }

        unsafe fn enum_nicks(&self, property: &GParamSpec) -> Result<Vec<String>> {
            ensure!(
                unsafe { (self.g_type_fundamental)(property.value_type) } == G_TYPE_ENUM,
                "encoder rate-control property is not an enum"
            );
            let class = unsafe { (self.g_type_class_ref)(property.value_type) };
            ensure!(
                !class.is_null(),
                "loading encoder rate-control enum metadata"
            );
            let class = class.cast::<GEnumClass>();
            let count = unsafe { (*class).n_values as usize };
            ensure!(
                count <= 1024,
                "encoder rate-control enum is unreasonably large"
            );
            let values = unsafe { std::slice::from_raw_parts((*class).values, count) };
            let mut nicks = Vec::with_capacity(values.len());
            for value in values {
                if !value.value_nick.is_null() {
                    nicks.push(
                        unsafe { CStr::from_ptr(value.value_nick) }
                            .to_str()
                            .context("encoder rate-control nickname is not UTF-8")?
                            .to_owned(),
                    );
                }
            }
            unsafe { (self.g_type_class_unref)(class.cast()) };
            Ok(nicks)
        }
    }

    fn ensure_non_null_string(value: *const c_char, name: &CStr) -> Result<String> {
        ensure!(
            !value.is_null(),
            "encoder {} property is null",
            name.to_string_lossy()
        );
        Ok(unsafe { CStr::from_ptr(value) }
            .to_str()
            .with_context(|| format!("encoder {} property is not UTF-8", name.to_string_lossy()))?
            .to_owned())
    }

    pub(super) fn probe(
        factory: &str,
        expected_device_path: &Path,
        rate_controls: &[VaapiRateControl],
    ) -> Result<()> {
        let factory = CString::new(factory).context("VA encoder factory contains NUL")?;
        let expected_device_path = expected_device_path
            .to_str()
            .context("VAAPI render node is not UTF-8")?;
        let device_path = c"device-path";
        let rate_control = c"rate-control";

        // SAFETY: all interaction is through the versioned public GStreamer and
        // GObject C ABIs. Host preflight invokes this probe in a bounded child,
        // so plugin faults cannot unwind through the daemon.
        unsafe {
            let api = GstreamerApi::load()?;
            let mut error = ptr::null_mut();
            if (api.gst_init_check)(ptr::null_mut(), ptr::null_mut(), &mut error) == 0 {
                let message = if error.is_null() || (*error).message.is_null() {
                    "unknown GStreamer initialization error".to_owned()
                } else {
                    CStr::from_ptr((*error).message)
                        .to_string_lossy()
                        .into_owned()
                };
                if !error.is_null() {
                    (api.g_error_free)(error);
                }
                anyhow::bail!("initializing GStreamer: {message}");
            }

            let element = (api.gst_element_factory_make)(factory.as_ptr(), ptr::null());
            ensure!(
                !element.is_null(),
                "creating configured VA encoder factory {:?}",
                factory.to_string_lossy()
            );

            let result = (|| {
                let device_property = api.property(element, device_path)?;
                ensure!(
                    device_property.flags & G_PARAM_READABLE != 0,
                    "encoder device-path property is not readable"
                );
                let observed_device_path =
                    api.string_property(element, device_path, device_property)?;
                ensure!(
                    observed_device_path == expected_device_path,
                    "configured VA encoder factory {:?} uses device-path {observed_device_path:?}, expected {expected_device_path:?}; on multi-GPU hosts select the per-device GstVA factory reported for that render node",
                    factory.to_string_lossy()
                );

                for property_name in [
                    c"aud",
                    c"b-frames",
                    c"key-int-max",
                    rate_control,
                    c"ref-frames",
                    c"target-usage",
                ] {
                    let property = api.property(element, property_name)?;
                    ensure!(
                        property.flags & G_PARAM_WRITABLE != 0,
                        "encoder {} property is not writable",
                        property_name.to_string_lossy()
                    );
                }

                let rate_control_property = api.property(element, rate_control)?;
                let rate_control_nicks = api.enum_nicks(rate_control_property)?;
                for required in rate_controls {
                    let (nickname, mode_properties): (&str, &[&CStr]) = match required {
                        VaapiRateControl::Cbr => ("cbr", &[c"bitrate"]),
                        VaapiRateControl::Cqp => ("cqp", &[c"qpi", c"qpp"]),
                    };
                    ensure!(
                        rate_control_nicks.iter().any(|value| value == nickname),
                        "configured VA encoder factory {:?} does not support {nickname} rate control",
                        factory.to_string_lossy()
                    );
                    for property_name in mode_properties {
                        let property = api.property(element, property_name)?;
                        ensure!(
                            property.flags & G_PARAM_WRITABLE != 0,
                            "encoder {} property is not writable",
                            property_name.to_string_lossy()
                        );
                    }
                }
                Ok(())
            })();
            (api.gst_object_unref)(element);
            result
        }
    }
}

pub(crate) fn probe_encoder_properties(
    factory: &str,
    expected_device_path: &Path,
    rate_controls: &[VaapiRateControl],
) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::probe(factory, expected_device_path, rate_controls)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (factory, expected_device_path, rate_controls);
        anyhow::bail!("GstVA encoder property probing requires Linux")
    }
}
