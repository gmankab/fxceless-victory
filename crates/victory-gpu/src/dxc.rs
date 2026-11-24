use windows::core::{HSTRING, PCSTR, PCWSTR, s};

/// Where `dxcompiler.dll` is looked for: `VICTORY_DXC` (the DLL or its
/// folder), beside the `dxil.dll` shader validation uses (the SDK ships them
/// together), then the executable's folder and System32.
fn dxc_library() -> windows::core::Result<windows::Win32::Foundation::HMODULE> {
    use windows::Win32::System::LibraryLoader::{
        LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
    };
    use windows::core::{HSTRING, w};
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("VICTORY_DXC").map(std::path::PathBuf::from) {
        paths.push(if p.is_dir() {
            p.join("dxcompiler.dll")
        } else {
            p
        });
    }
    if let Some(dir) = victory_shader::emit::validator::Validator::default_path()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        paths.push(dir.join("dxcompiler.dll"));
    }
    for path in paths.iter().filter(|p| p.is_file()) {
        // SAFETY: a NUL-terminated path; its folder (dxil.dll) is searched first.
        if let Ok(m) = unsafe {
            LoadLibraryExW(
                &HSTRING::from(path.as_path()),
                None,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        } {
            return Ok(m);
        }
    }
    // SAFETY: a bare name, searched only in the executable's folder and System32.
    unsafe {
        LoadLibraryExW(
            w!("dxcompiler.dll"),
            None,
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    }
}

/// Compiles a runtime shader as shader model 6.0 with DXC.
pub(crate) fn compile(source: &str, entry: PCSTR, target: PCSTR) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::Foundation::E_INVALIDARG;
    use windows::Win32::Graphics::Direct3D::Dxc::*;
    use windows::Win32::System::LibraryLoader::GetProcAddress;
    use windows::core::{Interface, w};
    type Create = unsafe extern "system" fn(
        *const windows::core::GUID,
        *const windows::core::GUID,
        *mut *mut std::ffi::c_void,
    ) -> windows::core::HRESULT;
    // SAFETY: loading the compiler and calling its documented entry point with
    // out pointers of ours; the buffer and arguments outlive the calls.
    unsafe {
        let entry = HSTRING::from(entry.to_string()?);
        let target = target.to_string()?;
        let stage = target.strip_suffix("_5_1").ok_or_else(|| {
            windows::core::Error::new(E_INVALIDARG, "expected a shader model 5.1 profile")
        })?;
        let profile = HSTRING::from(format!("{stage}_6_0"));
        let library = dxc_library()?;
        let create: Create = std::mem::transmute(
            GetProcAddress(library, s!("DxcCreateInstance"))
                .ok_or_else(windows::core::Error::from_thread)?,
        );
        let mut raw = std::ptr::null_mut();
        create(&CLSID_DxcCompiler, &IDxcCompiler3::IID, &mut raw).ok()?;
        let compiler = IDxcCompiler3::from_raw(raw);
        let buffer = DxcBuffer {
            Ptr: source.as_ptr().cast(),
            Size: source.len(),
            Encoding: DXC_CP_UTF8.0,
        };
        let arguments = [
            w!("-T"),
            PCWSTR(profile.as_ptr()),
            w!("-E"),
            PCWSTR(entry.as_ptr()),
            w!("-O3"),
        ];
        let result: IDxcResult = compiler.Compile(&buffer, Some(&arguments), None)?;
        if let Err(e) = result.GetStatus()?.ok() {
            if let Ok(errors) = result.GetErrorBuffer() {
                let text = std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                );
                tracing::warn!(
                    "DXC: {}",
                    String::from_utf8_lossy(text).trim()
                );
            }
            return Err(e);
        }
        let blob = result.GetResult()?;
        let mut bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
                .to_vec();
        sign_dxc_bytecode(library, &mut bytes)?;
        Ok(bytes)
    }
}

/// Older DXC DLLs can compile successfully without finding the signing DLL.
/// The resulting container has a zero digest and native D3D12 rejects it.
fn sign_dxc_bytecode(
    compiler: windows::Win32::Foundation::HMODULE,
    bytes: &mut [u8],
) -> windows::core::Result<()> {
    use std::os::windows::ffi::OsStringExt;
    use victory_shader::emit::validator::Validator;
    use windows::Win32::Foundation::E_FAIL;
    use windows::Win32::System::LibraryLoader::GetModuleFileNameW;

    if bytes.get(4..20).is_some_and(|digest| digest.iter().any(|&b| b != 0)) {
        return Ok(());
    }

    let explicit = std::env::var_os("VICTORY_DXIL").map(std::path::PathBuf::from);
    let beside_compiler = || {
        let mut path = vec![0; 32_768];
        // SAFETY: the compiler module is loaded and the buffer is ours.
        let len = unsafe { GetModuleFileNameW(Some(compiler), &mut path) } as usize;
        if len == 0 || len >= path.len() {
            return None;
        }
        let compiler = std::path::PathBuf::from(std::ffi::OsString::from_wide(&path[..len]));
        let validator = compiler.with_file_name("dxil.dll");
        validator.is_file().then_some(validator)
    };
    let path = explicit.or_else(beside_compiler).or_else(Validator::default_path);
    Validator::load(path.as_deref())
        .and_then(|mut validator| validator.validate(bytes))
        .map_err(|error| windows::core::Error::new(E_FAIL, format!("cannot sign DXC shader: {error}")))
}
