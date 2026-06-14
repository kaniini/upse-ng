// SPDX-License-Identifier: LGPL-2.1-or-later
//! Panic-contained C adapter for the high-level `upse` facade.

#![allow(
    unsafe_code,
    non_camel_case_types,
    clippy::missing_safety_doc,
    clippy::too_many_arguments
)]

use std::{
    ffi::{CStr, CString, c_char, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    ptr, slice,
};

use upse::{
    AudioAction, AudioBlock, DependencyLimits, Limits, ParseLimits, Player, PlayerBuilder,
    PlayerError, RenderOutcome, ResolvedFile, Resolver, ResolverError,
};

/// Stable C operation result and error category.
pub type upse_result = i32;
/// Stable action returned by an audio callback.
pub type upse_audio_action = i32;
/// Stable bounded-render outcome category.
pub type upse_render_kind = u32;
/// Stable metadata field identifier.
pub type upse_metadata_field = u32;

/// Current configuration/header ABI version.
pub const UPSE_ABI_VERSION: u32 = 1;
/// Operation succeeded.
pub const UPSE_RESULT_OK: upse_result = 0;
/// A required pointer, size, enum, or UTF-8 string was invalid.
pub const UPSE_RESULT_INVALID_ARGUMENT: upse_result = -1;
/// A root path or dependency could not be read.
pub const UPSE_RESULT_IO: upse_result = -2;
/// A container, executable, or metadata value was malformed.
pub const UPSE_RESULT_FORMAT: upse_result = -3;
/// The parsed module requires an unavailable machine feature.
pub const UPSE_RESULT_UNSUPPORTED: upse_result = -4;
/// A configured parser, dependency, or callback bound was exceeded.
pub const UPSE_RESULT_LIMIT: upse_result = -5;
/// Guest execution or post-mixing failed.
pub const UPSE_RESULT_EMULATION: upse_result = -6;
/// Audio callback reported a failure.
pub const UPSE_RESULT_CALLBACK_ERROR: upse_result = -7;
/// A Rust panic was contained at the ABI boundary.
pub const UPSE_RESULT_INTERNAL: upse_result = -8;

/// Callback asks playback to continue.
pub const UPSE_CALLBACK_CONTINUE: upse_audio_action = 0;
/// Callback asks the current render call to stop gracefully.
pub const UPSE_CALLBACK_STOP: upse_audio_action = 1;
/// Callback reports a sink error.
pub const UPSE_CALLBACK_ERROR: upse_audio_action = 2;

/// Render delivered the requested frame count.
pub const UPSE_RENDER_COMPLETE: upse_render_kind = 0;
/// Render reached the declared length-plus-fade end.
pub const UPSE_RENDER_END: upse_render_kind = 1;
/// Callback stopped rendering after consuming its current block.
pub const UPSE_RENDER_STOPPED: upse_render_kind = 2;

/// Metadata field identifiers for [`upse_player_metadata`].
pub const UPSE_METADATA_TITLE: upse_metadata_field = 0;
/// Artist metadata field.
pub const UPSE_METADATA_ARTIST: upse_metadata_field = 1;
/// Game metadata field.
pub const UPSE_METADATA_GAME: upse_metadata_field = 2;
/// Year metadata field.
pub const UPSE_METADATA_YEAR: upse_metadata_field = 3;
/// Genre metadata field.
pub const UPSE_METADATA_GENRE: upse_metadata_field = 4;
/// Comment metadata field.
pub const UPSE_METADATA_COMMENT: upse_metadata_field = 5;
/// Copyright metadata field.
pub const UPSE_METADATA_COPYRIGHT: upse_metadata_field = 6;
/// Ripper metadata field.
pub const UPSE_METADATA_PSF_BY: upse_metadata_field = 7;

/// C callback invoked synchronously with borrowed stereo floating-point frames.
pub type upse_audio_callback = Option<
    unsafe extern "C" fn(
        userdata: *mut c_void,
        samples: *const f32,
        frames: usize,
    ) -> upse_audio_action,
>;

/// Releases a resolver-returned borrowed blob exactly once.
pub type upse_blob_release =
    Option<unsafe extern "C" fn(userdata: *mut c_void, data: *const u8, size: usize)>;

/// Resolves one `_lib*` reference into a borrowed blob.
pub type upse_resolve_callback = Option<
    unsafe extern "C" fn(
        userdata: *mut c_void,
        containing_origin: *const c_char,
        reference: *const c_char,
        output: *mut upse_blob,
    ) -> upse_result,
>;

/// Sized root/parser/dependency configuration.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct upse_config {
    /// Must be `sizeof(upse_config)` or larger.
    pub size: u32,
    /// Must equal [`UPSE_ABI_VERSION`].
    pub abi_version: u32,
    /// Maximum frames passed to one audio callback.
    pub callback_quantum: u64,
    /// Maximum complete bytes in one PSF container.
    pub max_input_bytes: u64,
    /// Maximum reserved-section bytes.
    pub max_reserved_bytes: u64,
    /// Maximum decompressed program bytes.
    pub max_decompressed_bytes: u64,
    /// Maximum tag bytes.
    pub max_tag_bytes: u64,
    /// Maximum recursive dependency depth.
    pub max_dependency_depth: u64,
    /// Maximum total root/dependency file count.
    pub max_files: u64,
    /// Maximum aggregate root/dependency bytes.
    pub max_aggregate_bytes: u64,
}

impl Default for upse_config {
    fn default() -> Self {
        let limits = Limits::default();
        Self {
            size: u32::try_from(std::mem::size_of::<Self>()).unwrap_or(u32::MAX),
            abi_version: UPSE_ABI_VERSION,
            callback_quantum: 1024,
            max_input_bytes: u64::try_from(limits.parse.max_input_bytes).unwrap_or(u64::MAX),
            max_reserved_bytes: u64::try_from(limits.parse.max_reserved_bytes).unwrap_or(u64::MAX),
            max_decompressed_bytes: u64::try_from(limits.parse.max_decompressed_bytes)
                .unwrap_or(u64::MAX),
            max_tag_bytes: u64::try_from(limits.parse.max_tag_bytes).unwrap_or(u64::MAX),
            max_dependency_depth: u64::try_from(limits.dependencies.max_depth).unwrap_or(u64::MAX),
            max_files: u64::try_from(limits.dependencies.max_files).unwrap_or(u64::MAX),
            max_aggregate_bytes: u64::try_from(limits.dependencies.max_aggregate_bytes)
                .unwrap_or(u64::MAX),
        }
    }
}

/// Borrowed resolver result initialized by the C resolver callback.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct upse_blob {
    /// Must be `sizeof(upse_blob)` or larger.
    pub size: u32,
    /// Borrowed byte pointer; may be null only when `data_size` is zero.
    pub data: *const u8,
    /// Borrowed byte count.
    pub data_size: usize,
    /// Optional canonical UTF-8 logical origin; null uses the reference text.
    pub origin: *const c_char,
    /// Opaque value passed to `release`.
    pub userdata: *mut c_void,
    /// Optional one-shot release callback.
    pub release: upse_blob_release,
}

impl Default for upse_blob {
    fn default() -> Self {
        Self {
            size: u32::try_from(std::mem::size_of::<Self>()).unwrap_or(u32::MAX),
            data: ptr::null(),
            data_size: 0,
            origin: ptr::null(),
            userdata: ptr::null_mut(),
            release: None,
        }
    }
}

/// Sized custom dependency resolver vtable.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct upse_resolver {
    /// Must be `sizeof(upse_resolver)` or larger.
    pub size: u32,
    /// Opaque value passed to `resolve`.
    pub userdata: *mut c_void,
    /// Required resolver callback.
    pub resolve: upse_resolve_callback,
}

/// Sized native audio format result.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct upse_audio_format {
    /// Caller initializes to `sizeof(upse_audio_format)`.
    pub size: u32,
    /// Native frames per second.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u32,
}

/// Sized bounded-render result.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct upse_render_outcome {
    /// Caller initializes to `sizeof(upse_render_outcome)`.
    pub size: u32,
    /// One of the `UPSE_RENDER_*` constants.
    pub kind: upse_render_kind,
    /// Frames consumed by the current call.
    pub frames: u64,
}

/// Opaque owned error handle.
pub struct upse_error {
    code: upse_result,
    message: CString,
}

/// Opaque owned player handle.
pub struct upse_player {
    player: Player,
    strings: [Option<CString>; 8],
}

#[derive(Clone, Copy)]
struct CallbackBinding {
    callback: upse_audio_callback,
    userdata: usize,
}

impl CallbackBinding {
    fn call(self, block: AudioBlock<'_>) -> AudioAction {
        let Some(callback) = self.callback else {
            return AudioAction::Continue;
        };
        let result = unsafe {
            callback(
                self.userdata as *mut c_void,
                block.samples().as_ptr(),
                block.frames(),
            )
        };
        match result {
            UPSE_CALLBACK_CONTINUE => AudioAction::Continue,
            UPSE_CALLBACK_STOP => AudioAction::Stop,
            _ => AudioAction::Error,
        }
    }
}

#[derive(Clone, Copy)]
struct CResolver {
    resolver: upse_resolver,
}

struct BlobGuard(upse_blob);

impl Drop for BlobGuard {
    fn drop(&mut self) {
        if let Some(release) = self.0.release {
            unsafe { release(self.0.userdata, self.0.data, self.0.data_size) };
        }
    }
}

impl Resolver for CResolver {
    fn resolve(
        &mut self,
        containing_origin: &str,
        reference: &str,
    ) -> Result<ResolvedFile, ResolverError> {
        let callback = self
            .resolver
            .resolve
            .ok_or_else(|| ResolverError::new("C resolver has no resolve callback"))?;
        let containing = CString::new(containing_origin)
            .map_err(|_| ResolverError::new("containing origin contains a null byte"))?;
        let reference_c = CString::new(reference)
            .map_err(|_| ResolverError::new("dependency reference contains a null byte"))?;
        let mut blob = upse_blob::default();
        let result = unsafe {
            callback(
                self.resolver.userdata,
                containing.as_ptr(),
                reference_c.as_ptr(),
                &raw mut blob,
            )
        };
        let blob = BlobGuard(blob);
        if result != UPSE_RESULT_OK {
            return Err(ResolverError::new(format!(
                "C resolver rejected dependency {reference} with result {result}"
            )));
        }
        let (origin, bytes) = copy_blob(&blob.0, reference)?;
        Ok(ResolvedFile::new(origin, bytes))
    }
}

/// Returns the C ABI version implemented by this library.
#[unsafe(no_mangle)]
pub extern "C" fn upse_abi_version() -> u32 {
    catch_unwind(|| UPSE_ABI_VERSION).unwrap_or(0)
}

fn copy_blob(blob: &upse_blob, reference: &str) -> Result<(String, Vec<u8>), ResolverError> {
    require_size::<upse_blob>(blob.size, "upse_blob").map_err(ResolverError::new)?;
    if blob.data.is_null() && blob.data_size != 0 {
        return Err(ResolverError::new(
            "C resolver returned a null nonempty blob",
        ));
    }
    let bytes = if blob.data_size == 0 {
        Vec::new()
    } else {
        unsafe { slice::from_raw_parts(blob.data, blob.data_size) }.to_vec()
    };
    let origin = if blob.origin.is_null() {
        reference.to_owned()
    } else {
        unsafe { CStr::from_ptr(blob.origin) }
            .to_str()
            .map_err(|_| ResolverError::new("C resolver origin is not UTF-8"))?
            .to_owned()
    };
    Ok((origin, bytes))
}

/// Writes default configuration into a sized C structure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_config_init(config: *mut upse_config) -> upse_result {
    boundary(ptr::null_mut(), || {
        if config.is_null() {
            return Err(FfiError::invalid("config is null"));
        }
        unsafe { ptr::write(config, upse_config::default()) };
        Ok(())
    })
}

/// Opens root bytes and optional custom dependency resolver.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_player_open_memory(
    data: *const u8,
    data_size: usize,
    origin: *const c_char,
    config: *const upse_config,
    resolver: *const upse_resolver,
    output: *mut *mut upse_player,
    error: *mut *mut upse_error,
) -> upse_result {
    boundary(error, || {
        initialize_output(output)?;
        let bytes = borrowed_bytes(data, data_size)?;
        let origin = required_utf8(origin, "origin")?;
        let builder = configured_builder(config)?;
        let player = if resolver.is_null() {
            builder.open_memory(origin, bytes)
        } else {
            let resolver_value = read_sized(resolver, "upse_resolver")?;
            if resolver_value.resolve.is_none() {
                return Err(FfiError::invalid("resolver callback is null"));
            }
            let mut resolver = CResolver {
                resolver: resolver_value,
            };
            builder.open_with_resolver(origin, bytes, &mut resolver)
        }
        .map_err(|error| FfiError::player(&error))?;
        unsafe { ptr::write(output, Box::into_raw(Box::new(wrap_player(player)))) };
        Ok(())
    })
}

/// Opens a UTF-8 filesystem path using relative dependency resolution.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_player_open_path(
    path: *const c_char,
    config: *const upse_config,
    output: *mut *mut upse_player,
    error: *mut *mut upse_error,
) -> upse_result {
    boundary(error, || {
        initialize_output(output)?;
        let path = required_utf8(path, "path")?;
        let player = configured_builder(config)?
            .open_path(PathBuf::from(path))
            .map_err(|error| FfiError::player(&error))?;
        unsafe { ptr::write(output, Box::into_raw(Box::new(wrap_player(player)))) };
        Ok(())
    })
}

/// Replaces the synchronous audio callback; null installs a discard callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_player_set_callback(
    player: *mut upse_player,
    callback: upse_audio_callback,
    userdata: *mut c_void,
    error: *mut *mut upse_error,
) -> upse_result {
    boundary(error, || {
        let player = player_mut(player)?;
        let binding = CallbackBinding {
            callback,
            userdata: userdata as usize,
        };
        player.player.set_callback(move |block| binding.call(block));
        Ok(())
    })
}

/// Advances at most `max_frames` and invokes the installed callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_player_render(
    player: *mut upse_player,
    max_frames: u64,
    outcome: *mut upse_render_outcome,
    error: *mut *mut upse_error,
) -> upse_result {
    boundary(error, || {
        let player = player_mut(player)?;
        if outcome.is_null() {
            return Err(FfiError::invalid("render outcome is null"));
        }
        let size = unsafe { (*outcome).size };
        require_size::<upse_render_outcome>(size, "upse_render_outcome")
            .map_err(FfiError::invalid)?;
        let rendered = player
            .player
            .render(max_frames)
            .map_err(|error| FfiError::player(&error))?;
        let (kind, frames) = match rendered {
            RenderOutcome::Complete { frames } => (UPSE_RENDER_COMPLETE, frames),
            RenderOutcome::End { frames } => (UPSE_RENDER_END, frames),
            RenderOutcome::Stopped { frames } => (UPSE_RENDER_STOPPED, frames),
        };
        unsafe {
            (*outcome).kind = kind;
            (*outcome).frames = frames;
        }
        Ok(())
    })
}

/// Restores the opened module to frame zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_player_reset(
    player: *mut upse_player,
    error: *mut *mut upse_error,
) -> upse_result {
    boundary(error, || {
        player_mut(player)?.player.reset();
        Ok(())
    })
}

/// Writes native format information to a sized result structure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_player_audio_format(
    player: *const upse_player,
    output: *mut upse_audio_format,
    error: *mut *mut upse_error,
) -> upse_result {
    boundary(error, || {
        let player = player_ref(player)?;
        if output.is_null() {
            return Err(FfiError::invalid("audio format output is null"));
        }
        let size = unsafe { (*output).size };
        require_size::<upse_audio_format>(size, "upse_audio_format").map_err(FfiError::invalid)?;
        let format = player.player.audio_format();
        unsafe {
            (*output).sample_rate = format.sample_rate();
            (*output).channels = u32::from(format.channels());
        }
        Ok(())
    })
}

/// Returns a borrowed UTF-8 metadata string or null when absent/invalid field.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_player_metadata(
    player: *const upse_player,
    field: upse_metadata_field,
) -> *const c_char {
    pointer_boundary(|| {
        let player = player_ref(player).ok()?;
        player
            .strings
            .get(usize::try_from(field).ok()?)?
            .as_ref()
            .map_or(ptr::null(), |text| text.as_ptr())
            .into()
    })
}

/// Returns the parsed post-mix volume or zero for a null player.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_player_volume(player: *const upse_player) -> f64 {
    catch_unwind(AssertUnwindSafe(|| {
        player_ref(player).map_or(0.0, |player| player.player.metadata().volume)
    }))
    .unwrap_or(0.0)
}

/// Writes exact native length frames and returns one when the tag is present.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_player_length_frames(
    player: *const upse_player,
    output: *mut u64,
) -> i32 {
    optional_duration_frames(player, output, true)
}

/// Writes exact native fade frames and returns one for a valid player.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_player_fade_frames(
    player: *const upse_player,
    output: *mut u64,
) -> i32 {
    optional_duration_frames(player, output, false)
}

/// Returns frames delivered since open/reset, or zero for a null player.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_player_frames_rendered(player: *const upse_player) -> u64 {
    catch_unwind(AssertUnwindSafe(|| {
        player_ref(player).map_or(0, |player| player.player.frames_rendered())
    }))
    .unwrap_or(0)
}

/// Frees an owned player handle; null is accepted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_player_free(player: *mut upse_player) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !player.is_null() {
            unsafe { drop(Box::from_raw(player)) };
        }
    }));
}

/// Returns the borrowed error message or null for a null handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_error_message(error: *const upse_error) -> *const c_char {
    pointer_boundary(|| {
        if error.is_null() {
            None
        } else {
            Some(unsafe { (*error).message.as_ptr() })
        }
    })
}

/// Returns the stable result code stored in an error handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_error_code(error: *const upse_error) -> upse_result {
    catch_unwind(AssertUnwindSafe(|| {
        if error.is_null() {
            UPSE_RESULT_INVALID_ARGUMENT
        } else {
            unsafe { (*error).code }
        }
    }))
    .unwrap_or(UPSE_RESULT_INTERNAL)
}

/// Frees an owned error handle; null is accepted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn upse_error_free(error: *mut upse_error) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !error.is_null() {
            unsafe { drop(Box::from_raw(error)) };
        }
    }));
}

fn configured_builder(config: *const upse_config) -> Result<PlayerBuilder, FfiError> {
    let config = if config.is_null() {
        upse_config::default()
    } else {
        read_sized(config, "upse_config")?
    };
    if config.abi_version != UPSE_ABI_VERSION {
        return Err(FfiError::invalid(format!(
            "unsupported ABI version {}",
            config.abi_version
        )));
    }
    let quantum = to_usize(config.callback_quantum, "callback quantum")?;
    let limits = Limits {
        parse: ParseLimits {
            max_input_bytes: to_usize(config.max_input_bytes, "input byte limit")?,
            max_reserved_bytes: to_usize(config.max_reserved_bytes, "reserved byte limit")?,
            max_decompressed_bytes: to_usize(
                config.max_decompressed_bytes,
                "decompressed byte limit",
            )?,
            max_tag_bytes: to_usize(config.max_tag_bytes, "tag byte limit")?,
        },
        dependencies: DependencyLimits {
            max_depth: to_usize(config.max_dependency_depth, "dependency depth")?,
            max_files: to_usize(config.max_files, "file count")?,
            max_aggregate_bytes: to_usize(config.max_aggregate_bytes, "aggregate byte limit")?,
        },
        maximum_quantum: 65_536,
    };
    Ok(PlayerBuilder::new()
        .limits(limits)
        .callback_quantum(quantum))
}

fn wrap_player(player: Player) -> upse_player {
    let metadata = player.metadata();
    let strings = [
        c_string(metadata.title.as_deref()),
        c_string(metadata.artist.as_deref()),
        c_string(metadata.game.as_deref()),
        c_string(metadata.year.as_deref()),
        c_string(metadata.genre.as_deref()),
        c_string(metadata.comment.as_deref()),
        c_string(metadata.copyright.as_deref()),
        c_string(metadata.psfby.as_deref()),
    ];
    upse_player { player, strings }
}

fn c_string(value: Option<&str>) -> Option<CString> {
    value.and_then(|value| CString::new(value).ok())
}

fn optional_duration_frames(player: *const upse_player, output: *mut u64, length: bool) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if player.is_null() || output.is_null() {
            return 0;
        }
        let player = unsafe { &*player };
        let duration = if length {
            player.player.metadata().length
        } else {
            Some(player.player.metadata().fade)
        };
        let Some(duration) = duration else {
            return 0;
        };
        let Ok(frames) = duration.to_frames_floor(player.player.audio_format().sample_rate())
        else {
            return 0;
        };
        unsafe { ptr::write(output, frames) };
        1
    }))
    .unwrap_or(0)
}

fn initialize_output(output: *mut *mut upse_player) -> Result<(), FfiError> {
    if output.is_null() {
        return Err(FfiError::invalid("player output is null"));
    }
    unsafe { ptr::write(output, ptr::null_mut()) };
    Ok(())
}

fn player_mut<'a>(player: *mut upse_player) -> Result<&'a mut upse_player, FfiError> {
    if player.is_null() {
        return Err(FfiError::invalid("player is null"));
    }
    Ok(unsafe { &mut *player })
}

fn player_ref<'a>(player: *const upse_player) -> Result<&'a upse_player, FfiError> {
    if player.is_null() {
        return Err(FfiError::invalid("player is null"));
    }
    Ok(unsafe { &*player })
}

fn borrowed_bytes<'a>(data: *const u8, size: usize) -> Result<&'a [u8], FfiError> {
    if data.is_null() {
        if size == 0 {
            return Ok(&[]);
        }
        return Err(FfiError::invalid("root data is null with nonzero size"));
    }
    Ok(unsafe { slice::from_raw_parts(data, size) })
}

fn required_utf8<'a>(value: *const c_char, name: &str) -> Result<&'a str, FfiError> {
    if value.is_null() {
        return Err(FfiError::invalid(format!("{name} is null")));
    }
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .map_err(|_| FfiError::invalid(format!("{name} is not UTF-8")))
}

fn to_usize(value: u64, name: &str) -> Result<usize, FfiError> {
    usize::try_from(value).map_err(|_| FfiError::invalid(format!("{name} does not fit size_t")))
}

fn require_size<T>(actual: u32, name: &str) -> Result<(), String> {
    let required = u32::try_from(std::mem::size_of::<T>()).unwrap_or(u32::MAX);
    if actual < required {
        return Err(format!(
            "{name} size {actual} is smaller than required {required}"
        ));
    }
    Ok(())
}

fn read_sized<T: Copy>(value: *const T, name: &str) -> Result<T, FfiError> {
    let size = unsafe { ptr::read(value.cast::<u32>()) };
    require_size::<T>(size, name).map_err(FfiError::invalid)?;
    Ok(unsafe { ptr::read(value) })
}

#[derive(Debug)]
struct FfiError {
    code: upse_result,
    message: String,
}

impl FfiError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: UPSE_RESULT_INVALID_ARGUMENT,
            message: message.into(),
        }
    }

    fn player(error: &PlayerError) -> Self {
        let code = match error {
            PlayerError::Io { .. } => UPSE_RESULT_IO,
            PlayerError::Load(_) | PlayerError::Duration(_) => UPSE_RESULT_FORMAT,
            PlayerError::UnsupportedVersion => UPSE_RESULT_UNSUPPORTED,
            PlayerError::InvalidQuantum { .. } => UPSE_RESULT_LIMIT,
            PlayerError::Machine(_) | PlayerError::PostMix(_) => UPSE_RESULT_EMULATION,
            PlayerError::Callback { .. } => UPSE_RESULT_CALLBACK_ERROR,
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

fn boundary(
    error_output: *mut *mut upse_error,
    operation: impl FnOnce() -> Result<(), FfiError>,
) -> upse_result {
    if !error_output.is_null() {
        unsafe { ptr::write(error_output, ptr::null_mut()) };
    }
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(())) => UPSE_RESULT_OK,
        Ok(Err(error)) => {
            write_error(error_output, error.code, &error.message);
            error.code
        }
        Err(_) => {
            write_error(
                error_output,
                UPSE_RESULT_INTERNAL,
                "panic contained at libupse-ng ABI boundary",
            );
            UPSE_RESULT_INTERNAL
        }
    }
}

fn write_error(output: *mut *mut upse_error, code: upse_result, message: &str) {
    if output.is_null() {
        return;
    }
    let sanitized = message.replace('\0', "�");
    let message = CString::new(sanitized).unwrap_or_default();
    unsafe {
        ptr::write(
            output,
            Box::into_raw(Box::new(upse_error { code, message })),
        );
    };
}

fn pointer_boundary(operation: impl FnOnce() -> Option<*const c_char>) -> *const c_char {
    catch_unwind(AssertUnwindSafe(operation))
        .ok()
        .flatten()
        .unwrap_or(ptr::null())
}

#[cfg(test)]
mod tests {
    use std::{ffi::CString, ptr};

    use upse_psf::{PsfBuilder, PsfVersion};

    use super::{
        UPSE_ABI_VERSION, UPSE_METADATA_TITLE, UPSE_RESULT_INVALID_ARGUMENT, UPSE_RESULT_OK,
        upse_abi_version, upse_audio_format, upse_config, upse_config_init, upse_error,
        upse_error_free, upse_player, upse_player_audio_format, upse_player_free,
        upse_player_metadata, upse_player_open_memory,
    };

    fn fixture() -> Vec<u8> {
        let mut exe = vec![0_u8; 0x808];
        exe[..8].copy_from_slice(b"PS-X EXE");
        exe[0x10..0x14].copy_from_slice(&0x8001_0000_u32.to_le_bytes());
        exe[0x18..0x1c].copy_from_slice(&0x8001_0000_u32.to_le_bytes());
        exe[0x1c..0x20].copy_from_slice(&8_u32.to_le_bytes());
        exe[0x30..0x34].copy_from_slice(&0x801f_ff00_u32.to_le_bytes());
        exe[0x4c..0x51].copy_from_slice(b"Japan");
        exe[0x800..0x804].copy_from_slice(&0x0800_4000_u32.to_le_bytes());
        PsfBuilder::new(PsfVersion::Psf1)
            .program(exe)
            .tag("title", "FFI synthetic")
            .tag("length", "0.001")
            .build()
    }

    #[test]
    fn nulls_sizes_and_open_errors_are_contained() {
        unsafe {
            assert_eq!(
                upse_config_init(ptr::null_mut()),
                UPSE_RESULT_INVALID_ARGUMENT
            );
            assert_eq!(upse_abi_version(), UPSE_ABI_VERSION);
            let config = upse_config {
                size: 1,
                ..upse_config::default()
            };
            let mut player: *mut upse_player = ptr::null_mut();
            let mut error: *mut upse_error = ptr::null_mut();
            let origin = CString::new("bad.psf").unwrap();
            assert_eq!(
                upse_player_open_memory(
                    ptr::null(),
                    0,
                    origin.as_ptr(),
                    &raw const config,
                    ptr::null(),
                    &raw mut player,
                    &raw mut error,
                ),
                UPSE_RESULT_INVALID_ARGUMENT
            );
            assert!(player.is_null());
            assert!(!error.is_null());
            upse_error_free(error);
        }
    }

    #[test]
    fn metadata_and_format_views_remain_owned_by_player() {
        let bytes = fixture();
        unsafe {
            let mut config = upse_config::default();
            assert_eq!(upse_config_init(&raw mut config), UPSE_RESULT_OK);
            assert_eq!(config.abi_version, UPSE_ABI_VERSION);
            let origin = CString::new("fixture.psf").unwrap();
            let mut player: *mut upse_player = ptr::null_mut();
            let mut error: *mut upse_error = ptr::null_mut();
            assert_eq!(
                upse_player_open_memory(
                    bytes.as_ptr(),
                    bytes.len(),
                    origin.as_ptr(),
                    &raw const config,
                    ptr::null(),
                    &raw mut player,
                    &raw mut error,
                ),
                UPSE_RESULT_OK
            );
            assert!(!player.is_null());
            assert!(error.is_null());
            let title = upse_player_metadata(player, UPSE_METADATA_TITLE);
            assert!(!title.is_null());
            assert_eq!(std::ffi::CStr::from_ptr(title).to_bytes(), b"FFI synthetic");
            let mut format = upse_audio_format {
                size: u32::try_from(std::mem::size_of::<upse_audio_format>()).unwrap(),
                ..upse_audio_format::default()
            };
            assert_eq!(
                upse_player_audio_format(player, &raw mut format, ptr::null_mut()),
                UPSE_RESULT_OK
            );
            assert_eq!((format.sample_rate, format.channels), (44_100, 2));
            upse_player_free(player);
            assert!(upse_player_metadata(ptr::null(), UPSE_METADATA_TITLE).is_null());
            upse_player_free(ptr::null_mut());
        }
    }
}
