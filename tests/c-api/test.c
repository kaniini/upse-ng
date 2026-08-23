/* SPDX-License-Identifier: LGPL-2.1-or-later */

#include <pthread.h>

#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <upse.h>

#define ARRAY_LENGTH(array) (sizeof(array) / sizeof((array)[0]))

struct bytes {
  uint8_t *data;
  size_t size;
};

struct collector {
  float *samples;
  size_t capacity;
  size_t count;
  size_t calls;
  upse_audio_action action;
};

struct resolver_context {
  const struct bytes *library;
  upse_result result;
  int malformed;
  int releases;
  int bad_request;
};

struct thread_context {
  const struct bytes *module;
  uint64_t hash;
  int failed;
};

static int check(int condition, const char *message) {
  if (!condition) {
    fprintf(stderr, "c-api test failure: %s\n", message);
    return 0;
  }
  return 1;
}

static struct bytes read_file(const char *path) {
  struct bytes result = {NULL, 0};
  FILE *file = fopen(path, "rb");
  long length;

  if (file == NULL || fseek(file, 0, SEEK_END) != 0 ||
      (length = ftell(file)) < 0 || fseek(file, 0, SEEK_SET) != 0) {
    if (file != NULL) {
      fclose(file);
    }
    return result;
  }
  result.size = (size_t)length;
  result.data = malloc(result.size == 0 ? 1 : result.size);
  if (result.data == NULL ||
      fread(result.data, 1, result.size, file) != result.size) {
    free(result.data);
    result.data = NULL;
    result.size = 0;
  }
  fclose(file);
  return result;
}

static upse_audio_action collect_audio(void *userdata, const float *samples,
                                       size_t frames) {
  struct collector *collector = userdata;
  size_t count;

  if (collector == NULL || (samples == NULL && frames != 0) ||
      frames > SIZE_MAX / 2) {
    return UPSE_CALLBACK_ERROR;
  }
  count = frames * 2;
  if (collector->count > collector->capacity ||
      count > collector->capacity - collector->count) {
    return UPSE_CALLBACK_ERROR;
  }
  memcpy(collector->samples + collector->count, samples,
         count * sizeof(*samples));
  collector->count += count;
  collector->calls += 1;
  return collector->action;
}

static upse_audio_action discard_audio(void *userdata, const float *samples,
                                       size_t frames) {
  (void)userdata;
  (void)samples;
  (void)frames;
  return UPSE_CALLBACK_CONTINUE;
}

static void release_blob(void *userdata, const uint8_t *data, size_t size) {
  struct resolver_context *context = userdata;
  (void)data;
  (void)size;
  context->releases += 1;
}

static upse_result resolve_library(void *userdata,
                                   const char *containing_origin,
                                   const char *reference, upse_blob *output) {
  static const uint8_t malformed[] = {0, 1, 2, 3};
  struct resolver_context *context = userdata;

  if (strcmp(containing_origin, "virtual/root.minipsf") != 0 ||
      strcmp(reference, "library.psflib") != 0) {
    context->bad_request = 1;
  }
  output->size = sizeof(*output);
  output->data = context->malformed ? malformed : context->library->data;
  output->data_size =
      context->malformed ? sizeof(malformed) : context->library->size;
  output->origin = "virtual/library.psflib";
  output->userdata = context;
  output->release = release_blob;
  return context->result;
}

static upse_player *open_memory(const struct bytes *module, size_t quantum) {
  upse_config config;
  upse_error *error = NULL;
  upse_player *player = NULL;
  upse_result result;

  if (upse_config_init(&config) != UPSE_RESULT_OK) {
    return NULL;
  }
  config.callback_quantum = quantum;
  result = upse_player_open_memory(module->data, module->size, "synthetic.psf",
                                   &config, NULL, &player, &error);
  if (result != UPSE_RESULT_OK) {
    fprintf(stderr, "open failed: %s\n",
            error == NULL ? "no diagnostic" : upse_error_message(error));
    upse_error_free(error);
    return NULL;
  }
  return player;
}

static int test_success_and_callbacks(const struct bytes *module,
                                      const char *path) {
  float storage[512];
  struct collector collector = {storage, ARRAY_LENGTH(storage), 0, 0,
                                UPSE_CALLBACK_CONTINUE};
  upse_player *player = NULL;
  upse_error *error = NULL;
  upse_audio_format format = {sizeof(format), 0, 0};
  upse_render_outcome outcome = {sizeof(outcome), 0, 0};
  upse_config config;
  upse_result result;
  uint64_t frames;

  if (!check(upse_abi_version() == UPSE_ABI_VERSION, "ABI version") ||
      !check(upse_config_init(&config) == UPSE_RESULT_OK, "config init")) {
    return 0;
  }
  config.callback_quantum = 32;
  result = upse_player_open_path(path, &config, &player, &error);
  if (!check(result == UPSE_RESULT_OK && player != NULL && error == NULL,
             "path open") ||
      !check(upse_player_audio_format(player, &format, &error) ==
                 UPSE_RESULT_OK &&
                 format.sample_rate == 44100 && format.channels == 2,
             "audio format") ||
      !check(strcmp(upse_player_metadata(player, UPSE_METADATA_TITLE),
                    "UPSE-NG synthetic noise") == 0,
             "metadata") ||
      !check(upse_player_length_frames(player, &frames) && frames == 882,
             "length frames") ||
      !check(upse_player_fade_frames(player, &frames) && frames == 220,
             "fade frames")) {
    upse_error_free(error);
    upse_player_free(player);
    return 0;
  }
  result = upse_player_set_callback(player, collect_audio, &collector, &error);
  if (!check(result == UPSE_RESULT_OK, "callback install")) {
    upse_error_free(error);
    upse_player_free(player);
    return 0;
  }
  collector.action = UPSE_CALLBACK_STOP;
  result = upse_player_render(player, 100, &outcome, &error);
  if (!check(result == UPSE_RESULT_OK && outcome.kind == UPSE_RENDER_STOPPED &&
                 outcome.frames == 32 && collector.calls == 1,
             "callback stop")) {
    upse_error_free(error);
    upse_player_free(player);
    return 0;
  }
  collector.action = UPSE_CALLBACK_ERROR;
  collector.count = 0;
  collector.calls = 0;
  outcome.size = sizeof(outcome);
  result = upse_player_render(player, 1, &outcome, &error);
  if (!check(result == UPSE_RESULT_CALLBACK_ERROR && error != NULL &&
                 upse_error_code(error) == UPSE_RESULT_CALLBACK_ERROR,
             "callback error")) {
    upse_error_free(error);
    upse_player_free(player);
    return 0;
  }
  upse_error_free(error);
  error = NULL;
  upse_player_reset(player, &error);
  if (!check(error == NULL && upse_player_frames_rendered(player) == 0,
             "reset after callback error")) {
    upse_error_free(error);
    upse_player_free(player);
    return 0;
  }
  upse_player_free(player);

  player = open_memory(module, 17);
  if (!check(player != NULL, "memory open")) {
    return 0;
  }
  upse_player_free(player);
  return 1;
}

static int test_nulls_and_sizes(void) {
  upse_config config;
  upse_audio_format format = {sizeof(format), 0, 0};
  upse_error *error = NULL;
  upse_player *player = NULL;
  upse_result result;

  if (!check(upse_config_init(NULL) == UPSE_RESULT_INVALID_ARGUMENT,
             "null config") ||
      !check(upse_player_metadata(NULL, UPSE_METADATA_TITLE) == NULL,
             "null metadata") ||
      !check(upse_error_code(NULL) == UPSE_RESULT_INVALID_ARGUMENT,
             "null error code")) {
    return 0;
  }
  upse_config_init(&config);
  config.size = 1;
  result = upse_player_open_memory(NULL, 0, "invalid.psf", &config, NULL,
                                   &player, &error);
  if (!check(result == UPSE_RESULT_INVALID_ARGUMENT && error != NULL,
             "undersized config")) {
    upse_error_free(error);
    return 0;
  }
  upse_error_free(error);
  error = NULL;
  upse_config_init(&config);
  result = upse_player_open_memory(NULL, 0, "invalid.psf", &config, NULL,
                                   NULL, &error);
  if (!check(result == UPSE_RESULT_INVALID_ARGUMENT && error != NULL,
             "null player output")) {
    upse_error_free(error);
    return 0;
  }
  upse_error_free(error);
  error = NULL;
  if (!check(upse_player_audio_format(NULL, &format, &error) ==
                 UPSE_RESULT_INVALID_ARGUMENT &&
                 error != NULL,
             "null player")) {
    upse_error_free(error);
    return 0;
  }
  upse_error_free(error);
  upse_error_free(NULL);
  upse_player_free(NULL);
  return 1;
}

static int open_with_context(const struct bytes *root,
                             struct resolver_context *context,
                             upse_result expected) {
  upse_resolver resolver = {sizeof(resolver), context, resolve_library};
  upse_player *player = NULL;
  upse_error *error = NULL;
  upse_result result = upse_player_open_memory(
      root->data, root->size, "virtual/root.minipsf", NULL, &resolver, &player,
      &error);
  if (result != expected) {
    fprintf(stderr, "resolver open diagnostic: %s\n",
            error == NULL ? "no diagnostic" : upse_error_message(error));
  }
  int passed = check(result == expected, "resolver open result") &&
               check(context->releases == 1, "resolver one-shot release") &&
               check(!context->bad_request, "resolver request values");
  if (expected != UPSE_RESULT_OK) {
    passed = passed && check(error != NULL && upse_error_code(error) == expected,
                             "resolver error category");
  }
  upse_error_free(error);
  upse_player_free(player);
  return passed;
}

static int test_resolver_ownership(const struct bytes *root,
                                   const struct bytes *library) {
  struct resolver_context success = {library, UPSE_RESULT_OK, 0, 0, 0};
  struct resolver_context rejected = {library, UPSE_RESULT_IO, 0, 0, 0};
  struct resolver_context malformed = {library, UPSE_RESULT_OK, 1, 0, 0};

  return open_with_context(root, &success, UPSE_RESULT_OK) &&
         open_with_context(root, &rejected, UPSE_RESULT_IO) &&
         open_with_context(root, &malformed, UPSE_RESULT_FORMAT);
}

static int discard_frames(upse_player *player, uint64_t frames,
                          uint64_t partition) {
  upse_error *error = NULL;

  if (upse_player_set_callback(player, discard_audio, NULL, &error) !=
          UPSE_RESULT_OK ||
      upse_player_reset(player, &error) != UPSE_RESULT_OK) {
    upse_error_free(error);
    return 0;
  }
  while (frames != 0) {
    uint64_t request = frames < partition ? frames : partition;
    upse_render_outcome outcome = {sizeof(outcome), 0, 0};
    if (upse_player_render(player, request, &outcome, &error) !=
            UPSE_RESULT_OK ||
        outcome.frames != request || outcome.kind != UPSE_RENDER_COMPLETE) {
      upse_error_free(error);
      return 0;
    }
    frames -= request;
  }
  return 1;
}

static int collect_frames(upse_player *player, struct collector *collector,
                          uint64_t frames) {
  upse_error *error = NULL;
  upse_render_outcome outcome = {sizeof(outcome), 0, 0};
  int passed;

  collector->action = UPSE_CALLBACK_CONTINUE;
  if (upse_player_set_callback(player, collect_audio, collector, &error) !=
      UPSE_RESULT_OK) {
    upse_error_free(error);
    return 0;
  }
  passed = upse_player_render(player, frames, &outcome, &error) ==
               UPSE_RESULT_OK &&
           outcome.frames == frames && outcome.kind == UPSE_RENDER_COMPLETE;
  upse_error_free(error);
  return passed;
}

static int test_seek_equivalence(const struct bytes *module) {
  enum { TARGET = 97, WINDOW = 131 };
  float uninterrupted_samples[(TARGET + WINDOW) * 2];
  float first_seek_samples[WINDOW * 2];
  float second_seek_samples[WINDOW * 2];
  struct collector uninterrupted = {uninterrupted_samples,
                                    ARRAY_LENGTH(uninterrupted_samples), 0, 0,
                                    UPSE_CALLBACK_CONTINUE};
  struct collector first_seek = {first_seek_samples,
                                 ARRAY_LENGTH(first_seek_samples), 0, 0,
                                 UPSE_CALLBACK_CONTINUE};
  struct collector second_seek = {second_seek_samples,
                                  ARRAY_LENGTH(second_seek_samples), 0, 0,
                                  UPSE_CALLBACK_CONTINUE};
  upse_player *first = open_memory(module, 31);
  upse_player *second = open_memory(module, 31);
  upse_player *third = open_memory(module, 31);
  int passed;

  if (first == NULL || second == NULL || third == NULL) {
    upse_player_free(first);
    upse_player_free(second);
    upse_player_free(third);
    return 0;
  }
  passed = collect_frames(first, &uninterrupted, TARGET + WINDOW) &&
           discard_frames(second, TARGET, 13) &&
           collect_frames(second, &first_seek, WINDOW) &&
           discard_frames(third, TARGET, 29) &&
           collect_frames(third, &second_seek, WINDOW) &&
           check(uninterrupted.count == (TARGET + WINDOW) * 2,
                 "uninterrupted sample count") &&
           check(first_seek.count == WINDOW * 2 &&
                     second_seek.count == WINDOW * 2,
                 "seek sample counts") &&
           check(memcmp(uninterrupted.samples + TARGET * 2,
                        first_seek.samples,
                        sizeof(first_seek_samples)) == 0,
                 "seek equals uninterrupted output") &&
           check(memcmp(first_seek.samples, second_seek.samples,
                        sizeof(first_seek_samples)) == 0,
                 "discard partition independence");
  upse_player_free(first);
  upse_player_free(second);
  upse_player_free(third);
  return passed;
}

static uint64_t hash_samples(const float *samples, size_t count) {
  const unsigned char *bytes = (const unsigned char *)samples;
  uint64_t hash = UINT64_C(1469598103934665603);
  size_t index;

  for (index = 0; index < count * sizeof(*samples); ++index) {
    hash ^= bytes[index];
    hash *= UINT64_C(1099511628211);
  }
  return hash;
}

static void *thread_player(void *userdata) {
  struct thread_context *context = userdata;
  float samples[256];
  struct collector collector = {samples, ARRAY_LENGTH(samples), 0, 0,
                                UPSE_CALLBACK_CONTINUE};
  upse_player *player = open_memory(context->module, 23);

  if (player == NULL || !collect_frames(player, &collector, 128)) {
    context->failed = 1;
  } else {
    context->hash = hash_samples(samples, collector.count);
  }
  upse_player_free(player);
  return NULL;
}

static int player_thread_attributes(pthread_attr_t *attributes) {
  return pthread_attr_init(attributes) == 0 &&
         pthread_attr_setstacksize(attributes, 64U * 1024U) == 0;
}

static int test_parallel_handles(const struct bytes *module) {
  struct thread_context first = {module, 0, 0};
  struct thread_context second = {module, 0, 0};
  pthread_t first_thread;
  pthread_t second_thread;
  pthread_attr_t attributes;

  if (!player_thread_attributes(&attributes)) {
    return check(0, "pthread attributes");
  }
  if (pthread_create(&first_thread, &attributes, thread_player, &first) != 0) {
    pthread_attr_destroy(&attributes);
    return check(0, "pthread create");
  }
  if (pthread_create(&second_thread, &attributes, thread_player, &second) != 0) {
    pthread_attr_destroy(&attributes);
    pthread_join(first_thread, NULL);
    return check(0, "pthread create");
  }
  pthread_attr_destroy(&attributes);
  if (pthread_join(first_thread, NULL) != 0 ||
      pthread_join(second_thread, NULL) != 0) {
    return check(0, "pthread join");
  }
  return check(!first.failed && !second.failed, "parallel render") &&
         check(first.hash == second.hash, "parallel deterministic hash");
}

static int test_psf2(const struct bytes *module, const char *path) {
  float samples[256];
  struct collector collector = {samples, ARRAY_LENGTH(samples), 0, 0,
                                UPSE_CALLBACK_CONTINUE};
  upse_player *player = NULL;
  upse_error *error = NULL;
  upse_audio_format format = {sizeof(format), 0, 0};
  upse_render_outcome outcome = {sizeof(outcome), 0, 0};
  upse_config config;
  uint64_t frames;
  size_t index;
  int audible = 0;

  if (!check(upse_config_init(&config) == UPSE_RESULT_OK,
             "PSF2 config init")) {
    return 0;
  }
  config.callback_quantum = 64;
  if (!check(upse_player_open_path(path, &config, &player, &error) ==
                 UPSE_RESULT_OK &&
             player != NULL,
             "PSF2 path open") ||
      !check(upse_player_audio_format(player, &format, &error) ==
                     UPSE_RESULT_OK &&
                 format.sample_rate == 48000 && format.channels == 2,
             "PSF2 audio format") ||
      !check(upse_player_length_frames(player, &frames) && frames == 960,
             "PSF2 length frames") ||
      !check(upse_player_set_callback(player, collect_audio, &collector,
                                      &error) == UPSE_RESULT_OK,
             "PSF2 callback") ||
      !check(upse_player_render(player, 64, &outcome, &error) ==
                     UPSE_RESULT_OK &&
                 outcome.frames == 64,
             "PSF2 render")) {
    if (error != NULL) {
      fprintf(stderr, "PSF2 diagnostic: %s\n", upse_error_message(error));
    }
    upse_error_free(error);
    upse_player_free(player);
    return 0;
  }
  for (index = 0; index < collector.count; ++index) {
    if (collector.samples[index] != 0.0f) {
      audible = 1;
      break;
    }
  }
  upse_error_free(error);
  upse_player_free(player);
  return check(audible, "PSF2 non-silent output") &&
         check(module->size != 0, "PSF2 memory fixture");
}

static int test_parallel_formats(const struct bytes *psf1,
                                 const struct bytes *psf2) {
  struct thread_context first = {psf1, 0, 0};
  struct thread_context second = {psf2, 0, 0};
  pthread_t first_thread;
  pthread_t second_thread;
  pthread_attr_t attributes;

  if (!player_thread_attributes(&attributes)) {
    return check(0, "mixed-format pthread attributes");
  }
  if (pthread_create(&first_thread, &attributes, thread_player, &first) != 0) {
    pthread_attr_destroy(&attributes);
    return check(0, "mixed-format pthread create");
  }
  if (pthread_create(&second_thread, &attributes, thread_player, &second) != 0) {
    pthread_attr_destroy(&attributes);
    pthread_join(first_thread, NULL);
    return check(0, "mixed-format pthread create");
  }
  pthread_attr_destroy(&attributes);
  if (pthread_join(first_thread, NULL) != 0 ||
      pthread_join(second_thread, NULL) != 0) {
    return check(0, "mixed-format pthread join");
  }
  return check(!first.failed && !second.failed,
               "parallel PSF1 and PSF2 render");
}

int main(int argc, char **argv) {
  struct bytes module;
  struct bytes root;
  struct bytes library;
  struct bytes psf2_module;
  int passed;

  if (argc != 5) {
    fprintf(stderr,
            "usage: %s SYNTHETIC.psf SYNTHETIC.minipsf LIBRARY.psflib "
            "SYNTHETIC.psf2\n",
            argv[0]);
    return EXIT_FAILURE;
  }
  module = read_file(argv[1]);
  root = read_file(argv[2]);
  library = read_file(argv[3]);
  psf2_module = read_file(argv[4]);
  if (module.data == NULL || root.data == NULL || library.data == NULL ||
      psf2_module.data == NULL) {
    fprintf(stderr, "c-api test: cannot read generated fixtures\n");
    free(module.data);
    free(root.data);
    free(library.data);
    free(psf2_module.data);
    return EXIT_FAILURE;
  }
  passed = test_success_and_callbacks(&module, argv[1]) &&
           test_nulls_and_sizes() && test_resolver_ownership(&root, &library) &&
           test_seek_equivalence(&module) && test_parallel_handles(&module) &&
           test_psf2(&psf2_module, argv[4]) &&
           test_parallel_formats(&module, &psf2_module);
  free(module.data);
  free(root.data);
  free(library.data);
  free(psf2_module.data);
  if (!passed) {
    return EXIT_FAILURE;
  }
  puts("C API tests passed");
  return EXIT_SUCCESS;
}
