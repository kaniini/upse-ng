/* SPDX-License-Identifier: LGPL-2.1-or-later */

#ifndef UPSE_H
#define UPSE_H

#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#if defined(_WIN32) && defined(UPSE_NG_SHARED)
# if defined(UPSE_NG_BUILD)
#  define UPSE_API __declspec(dllexport)
# else
#  define UPSE_API __declspec(dllimport)
# endif
#elif defined(__GNUC__) && defined(UPSE_NG_BUILD)
# define UPSE_API __attribute__((visibility("default")))
#else
# define UPSE_API
#endif


/**
 * Current configuration/header ABI version.
 */
#define UPSE_ABI_VERSION 3

/**
 * Opaque owned error handle.
 */
typedef struct upse_error upse_error;

/**
 * Opaque owned player handle.
 */
typedef struct upse_player upse_player;

/**
 * Stable C operation result and error category.
 */
typedef int32_t upse_result;

/**
 * Stable post-emulation gain policy.
 */
typedef uint32_t upse_gain_policy;

/**
 * Stable length or fade duration policy.
 */
typedef uint32_t upse_duration_policy;

/**
 * Gain, length, and fade policy for one PSF format version.
 */
typedef struct upse_playback_config {
  /**
   * Gain coefficient used by [`UPSE_GAIN_OVERRIDE`].
   */
  double gain;
  /**
   * Length in milliseconds used by a configured duration policy.
   */
  uint64_t length_ms;
  /**
   * Fade in milliseconds used by a configured duration policy.
   */
  uint64_t fade_ms;
  /**
   * One of the `UPSE_GAIN_*` constants.
   */
  upse_gain_policy gain_policy;
  /**
   * One of the `UPSE_DURATION_*` constants.
   */
  upse_duration_policy length_policy;
  /**
   * One of the `UPSE_DURATION_*` constants.
   */
  upse_duration_policy fade_policy;
} upse_playback_config;

/**
 * Sized parser, dependency, and per-format playback configuration.
 */
typedef struct upse_config {
  /**
   * Must be `sizeof(upse_config)` or larger.
   */
  uint32_t size;
  /**
   * Must equal [`UPSE_ABI_VERSION`].
   */
  uint32_t abi_version;
  /**
   * Maximum frames passed to one audio callback.
   */
  uint64_t callback_quantum;
  /**
   * Maximum complete bytes in one PSF container.
   */
  uint64_t max_input_bytes;
  /**
   * Maximum reserved-section bytes.
   */
  uint64_t max_reserved_bytes;
  /**
   * Maximum decompressed program bytes.
   */
  uint64_t max_decompressed_bytes;
  /**
   * Maximum tag bytes.
   */
  uint64_t max_tag_bytes;
  /**
   * Maximum recursive dependency depth.
   */
  uint64_t max_dependency_depth;
  /**
   * Maximum total root/dependency file count.
   */
  uint64_t max_files;
  /**
   * Maximum aggregate root/dependency bytes.
   */
  uint64_t max_aggregate_bytes;
  /**
   * Consecutive quiet milliseconds which end playback; zero disables detection.
   */
  uint64_t trailing_silence_ms;
  /**
   * Maximum absolute normalized sample amplitude considered quiet.
   */
  float silence_threshold;
  /**
   * Gain and timeline policy selected for PSF1 files.
   */
  struct upse_playback_config psf1_playback;
  /**
   * Gain and timeline policy selected for PSF2 files.
   */
  struct upse_playback_config psf2_playback;
} upse_config;

/**
 * Releases a resolver-returned borrowed blob exactly once.
 */
typedef void (*upse_blob_release)(void *userdata, const uint8_t *data, size_t size);

/**
 * Borrowed resolver result initialized by the C resolver callback.
 */
typedef struct upse_blob {
  /**
   * Must be `sizeof(upse_blob)` or larger.
   */
  uint32_t size;
  /**
   * Borrowed byte pointer; may be null only when `data_size` is zero.
   */
  const uint8_t *data;
  /**
   * Borrowed byte count.
   */
  size_t data_size;
  /**
   * Optional canonical UTF-8 logical origin; null uses the reference text.
   */
  const char *origin;
  /**
   * Opaque value passed to `release`.
   */
  void *userdata;
  /**
   * Optional one-shot release callback.
   */
  upse_blob_release release;
} upse_blob;

/**
 * Resolves one `_lib*` reference into a borrowed blob.
 */
typedef upse_result (*upse_resolve_callback)(void *userdata,
                                             const char *containing_origin,
                                             const char *reference,
                                             struct upse_blob *output);

/**
 * Sized custom dependency resolver vtable.
 */
typedef struct upse_resolver {
  /**
   * Must be `sizeof(upse_resolver)` or larger.
   */
  uint32_t size;
  /**
   * Opaque value passed to `resolve`.
   */
  void *userdata;
  /**
   * Required resolver callback.
   */
  upse_resolve_callback resolve;
} upse_resolver;

/**
 * Stable action returned by an audio callback.
 */
typedef int32_t upse_audio_action;

/**
 * C callback invoked synchronously with borrowed stereo floating-point frames.
 */
typedef upse_audio_action (*upse_audio_callback)(void *userdata, const float *samples, size_t frames);

/**
 * Stable bounded-render outcome category.
 */
typedef uint32_t upse_render_kind;

/**
 * Sized bounded-render result.
 */
typedef struct upse_render_outcome {
  /**
   * Caller initializes to `sizeof(upse_render_outcome)`.
   */
  uint32_t size;
  /**
   * One of the `UPSE_RENDER_*` constants.
   */
  upse_render_kind kind;
  /**
   * Frames consumed by the current call.
   */
  uint64_t frames;
} upse_render_outcome;

/**
 * Sized native audio format result.
 */
typedef struct upse_audio_format {
  /**
   * Caller initializes to `sizeof(upse_audio_format)`.
   */
  uint32_t size;
  /**
   * Native frames per second.
   */
  uint32_t sample_rate;
  /**
   * Interleaved channel count.
   */
  uint32_t channels;
} upse_audio_format;

/**
 * Stable metadata field identifier.
 */
typedef uint32_t upse_metadata_field;

/**
 * Operation succeeded.
 */
#define UPSE_RESULT_OK 0

/**
 * A required pointer, size, enum, or UTF-8 string was invalid.
 */
#define UPSE_RESULT_INVALID_ARGUMENT -1

/**
 * A root path or dependency could not be read.
 */
#define UPSE_RESULT_IO -2

/**
 * A container, executable, or metadata value was malformed.
 */
#define UPSE_RESULT_FORMAT -3

/**
 * The parsed module requires an unavailable machine feature.
 */
#define UPSE_RESULT_UNSUPPORTED -4

/**
 * A configured parser, dependency, or callback bound was exceeded.
 */
#define UPSE_RESULT_LIMIT -5

/**
 * Guest execution or post-mixing failed.
 */
#define UPSE_RESULT_EMULATION -6

/**
 * Audio callback reported a failure.
 */
#define UPSE_RESULT_CALLBACK_ERROR -7

/**
 * A Rust panic was contained at the ABI boundary.
 */
#define UPSE_RESULT_INTERNAL -8

/**
 * Callback asks playback to continue.
 */
#define UPSE_CALLBACK_CONTINUE 0

/**
 * Callback asks the current render call to stop gracefully.
 */
#define UPSE_CALLBACK_STOP 1

/**
 * Callback reports a sink error.
 */
#define UPSE_CALLBACK_ERROR 2

/**
 * Render delivered the requested frame count.
 */
#define UPSE_RENDER_COMPLETE 0

/**
 * Render reached the configured length-plus-fade end.
 */
#define UPSE_RENDER_END 1

/**
 * Callback stopped rendering after consuming its current block.
 */
#define UPSE_RENDER_STOPPED 2

/**
 * Metadata field identifiers for [`upse_player_metadata`].
 */
#define UPSE_METADATA_TITLE 0

/**
 * Artist metadata field.
 */
#define UPSE_METADATA_ARTIST 1

/**
 * Game metadata field.
 */
#define UPSE_METADATA_GAME 2

/**
 * Year metadata field.
 */
#define UPSE_METADATA_YEAR 3

/**
 * Genre metadata field.
 */
#define UPSE_METADATA_GENRE 4

/**
 * Comment metadata field.
 */
#define UPSE_METADATA_COMMENT 5

/**
 * Copyright metadata field.
 */
#define UPSE_METADATA_COPYRIGHT 6

/**
 * Ripper metadata field.
 */
#define UPSE_METADATA_PSF_BY 7

/**
 * Apply the PSF `volume` tag.
 */
#define UPSE_GAIN_TAG 0

/**
 * Ignore the tag and apply the configured gain coefficient.
 */
#define UPSE_GAIN_OVERRIDE 1

/**
 * Use the PSF duration tag and its format-defined absent default.
 */
#define UPSE_DURATION_TAG 0

/**
 * Use the PSF duration tag or the configured fallback when absent.
 */
#define UPSE_DURATION_TAG_OR_DEFAULT 1

/**
 * Ignore the tag and use the configured duration.
 */
#define UPSE_DURATION_OVERRIDE 2

/**
 * Ignore the tag; length becomes indefinite and fade becomes zero.
 */
#define UPSE_DURATION_IGNORE 3

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/**
 * Returns the C ABI version implemented by this library.
 */
UPSE_API uint32_t upse_abi_version(void);

/**
 * Writes default configuration into a sized C structure.
 */
UPSE_API upse_result upse_config_init(struct upse_config *config);

/**
 * Opens root bytes and optional custom dependency resolver.
 */
UPSE_API
upse_result upse_player_open_memory(const uint8_t *data,
                                    size_t data_size,
                                    const char *origin,
                                    const struct upse_config *config,
                                    const struct upse_resolver *resolver,
                                    struct upse_player **output,
                                    struct upse_error **error);

/**
 * Opens a UTF-8 filesystem path using relative dependency resolution.
 */
UPSE_API
upse_result upse_player_open_path(const char *path,
                                  const struct upse_config *config,
                                  struct upse_player **output,
                                  struct upse_error **error);

/**
 * Replaces the synchronous audio callback; null installs a discard callback.
 */
UPSE_API
upse_result upse_player_set_callback(struct upse_player *player,
                                     upse_audio_callback callback,
                                     void *userdata,
                                     struct upse_error **error);

/**
 * Advances at most `max_frames` and invokes the installed callback.
 */
UPSE_API
upse_result upse_player_render(struct upse_player *player,
                               uint64_t max_frames,
                               struct upse_render_outcome *outcome,
                               struct upse_error **error);

/**
 * Advances at most `max_frames` without invoking the audio callback.
 */
UPSE_API
upse_result upse_player_advance(struct upse_player *player,
                                uint64_t max_frames,
                                struct upse_render_outcome *outcome,
                                struct upse_error **error);

/**
 * Restores the opened module to frame zero.
 */
UPSE_API upse_result upse_player_reset(struct upse_player *player, struct upse_error **error);

/**
 * Writes native format information to a sized result structure.
 */
UPSE_API
upse_result upse_player_audio_format(const struct upse_player *player,
                                     struct upse_audio_format *output,
                                     struct upse_error **error);

/**
 * Returns a borrowed UTF-8 metadata string or null when absent/invalid field.
 */
UPSE_API
const char *upse_player_metadata(const struct upse_player *player,
                                 upse_metadata_field field);

/**
 * Returns the parsed `volume` tag coefficient or zero for a null player.
 */
UPSE_API double upse_player_volume(const struct upse_player *player);

/**
 * Writes exact native length frames and returns one when the tag is present.
 */
UPSE_API int32_t upse_player_length_frames(const struct upse_player *player, uint64_t *output);

/**
 * Writes exact native fade frames and returns one for a valid player.
 */
UPSE_API int32_t upse_player_fade_frames(const struct upse_player *player, uint64_t *output);

/**
 * Returns the resolved gain or zero for a null player.
 */
UPSE_API double upse_player_effective_gain(const struct upse_player *player);

/**
 * Writes the resolved native length and returns one when playback is finite.
 */
UPSE_API
int32_t upse_player_effective_length_frames(const struct upse_player *player,
                                            uint64_t *output);

/**
 * Writes the resolved native fade and returns one for a valid player.
 */
UPSE_API
int32_t upse_player_effective_fade_frames(const struct upse_player *player,
                                          uint64_t *output);

/**
 * Returns timeline frames rendered or advanced since open/reset.
 */
UPSE_API uint64_t upse_player_frames_rendered(const struct upse_player *player);

/**
 * Frees an owned player handle; null is accepted.
 */
UPSE_API void upse_player_free(struct upse_player *player);

/**
 * Returns the borrowed error message or null for a null handle.
 */
UPSE_API const char *upse_error_message(const struct upse_error *error);

/**
 * Returns the stable result code stored in an error handle.
 */
UPSE_API upse_result upse_error_code(const struct upse_error *error);

/**
 * Frees an owned error handle; null is accepted.
 */
UPSE_API void upse_error_free(struct upse_error *error);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus

#endif  /* UPSE_H */
