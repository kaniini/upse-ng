/* SPDX-License-Identifier: LGPL-2.1-or-later */
#ifndef UPSE_H
#define UPSE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#if defined(_WIN32) && defined(UPSE_NG_SHARED)
# if defined(UPSE_NG_BUILD)
#  define UPSE_API __declspec(dllexport)
# else
#  define UPSE_API __declspec(dllimport)
# endif
#else
# define UPSE_API
#endif

#define UPSE_ABI_VERSION UINT32_C(1)

typedef int32_t upse_result;
typedef int32_t upse_callback_result;
typedef uint32_t upse_render_kind;
typedef uint32_t upse_metadata_field;

#define UPSE_RESULT_OK ((upse_result)0)
#define UPSE_RESULT_INVALID_ARGUMENT ((upse_result)-1)
#define UPSE_RESULT_IO ((upse_result)-2)
#define UPSE_RESULT_FORMAT ((upse_result)-3)
#define UPSE_RESULT_UNSUPPORTED ((upse_result)-4)
#define UPSE_RESULT_LIMIT ((upse_result)-5)
#define UPSE_RESULT_EMULATION ((upse_result)-6)
#define UPSE_RESULT_CALLBACK_ERROR ((upse_result)-7)
#define UPSE_RESULT_INTERNAL ((upse_result)-8)

#define UPSE_CALLBACK_CONTINUE ((upse_callback_result)0)
#define UPSE_CALLBACK_STOP ((upse_callback_result)1)
#define UPSE_CALLBACK_ERROR ((upse_callback_result)2)

#define UPSE_RENDER_COMPLETE ((upse_render_kind)0)
#define UPSE_RENDER_END ((upse_render_kind)1)
#define UPSE_RENDER_STOPPED ((upse_render_kind)2)

#define UPSE_METADATA_TITLE ((upse_metadata_field)0)
#define UPSE_METADATA_ARTIST ((upse_metadata_field)1)
#define UPSE_METADATA_GAME ((upse_metadata_field)2)
#define UPSE_METADATA_YEAR ((upse_metadata_field)3)
#define UPSE_METADATA_GENRE ((upse_metadata_field)4)
#define UPSE_METADATA_COMMENT ((upse_metadata_field)5)
#define UPSE_METADATA_COPYRIGHT ((upse_metadata_field)6)
#define UPSE_METADATA_PSF_BY ((upse_metadata_field)7)

typedef struct upse_player upse_player;
typedef struct upse_error upse_error;

typedef upse_callback_result (*upse_audio_callback)(void *userdata, const float *samples,
    size_t frames);
typedef void (*upse_blob_release)(void *userdata, const uint8_t *data,
    size_t size);

typedef struct upse_config {
    uint32_t size;
    uint32_t abi_version;
    uint64_t callback_quantum;
    uint64_t max_input_bytes;
    uint64_t max_reserved_bytes;
    uint64_t max_decompressed_bytes;
    uint64_t max_tag_bytes;
    uint64_t max_dependency_depth;
    uint64_t max_files;
    uint64_t max_aggregate_bytes;
} upse_config;

typedef struct upse_blob {
    uint32_t size;
    const uint8_t *data;
    size_t data_size;
    const char *origin;
    void *userdata;
    upse_blob_release release;
} upse_blob;

typedef upse_result (*upse_resolve_callback)(void *userdata,
    const char *containing_origin, const char *reference, upse_blob *output);

typedef struct upse_resolver {
    uint32_t size;
    void *userdata;
    upse_resolve_callback resolve;
} upse_resolver;

typedef struct upse_audio_format {
    uint32_t size;
    uint32_t sample_rate;
    uint32_t channels;
} upse_audio_format;

typedef struct upse_render_outcome {
    uint32_t size;
    uint32_t kind;
    uint64_t frames;
} upse_render_outcome;

UPSE_API uint32_t upse_abi_version(void);
UPSE_API upse_result upse_config_init(upse_config *config);
UPSE_API upse_result upse_player_open_memory(const uint8_t *data, size_t data_size,
    const char *origin, const upse_config *config,
    const upse_resolver *resolver, upse_player **output, upse_error **error);
UPSE_API upse_result upse_player_open_path(const char *path,
    const upse_config *config, upse_player **output, upse_error **error);
UPSE_API upse_result upse_player_set_callback(upse_player *player,
    upse_audio_callback callback, void *userdata, upse_error **error);
UPSE_API upse_result upse_player_render(upse_player *player, uint64_t max_frames,
    upse_render_outcome *outcome, upse_error **error);
UPSE_API upse_result upse_player_reset(upse_player *player, upse_error **error);
UPSE_API upse_result upse_player_audio_format(const upse_player *player,
    upse_audio_format *output, upse_error **error);
UPSE_API const char *upse_player_metadata(const upse_player *player,
    uint32_t field);
UPSE_API double upse_player_volume(const upse_player *player);
UPSE_API int32_t upse_player_length_frames(const upse_player *player,
    uint64_t *output);
UPSE_API int32_t upse_player_fade_frames(const upse_player *player,
    uint64_t *output);
UPSE_API uint64_t upse_player_frames_rendered(const upse_player *player);
UPSE_API void upse_player_free(upse_player *player);
UPSE_API upse_result upse_error_code(const upse_error *error);
UPSE_API const char *upse_error_message(const upse_error *error);
UPSE_API void upse_error_free(upse_error *error);

#ifdef __cplusplus
}
#endif

#endif
