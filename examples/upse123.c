/*
 * SPDX-License-Identifier: GPL-2.0-or-later
 *
 * Copyright (c) 2007 William Pitcock <nenolod@sacredspiral.co.uk>
 *
 * This player is derived from the original UPSE upse123 and has been
 * reworked to use the libupse-ng C interface and floating-point callbacks.
 */

#include <ao/ao.h>

#include <errno.h>
#include <getopt.h>
#include <inttypes.h>
#include <limits.h>
#include <math.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <upse.h>

#ifndef UPSE123_VERSION
#define UPSE123_VERSION "0.1.0"
#endif

#define RENDER_QUANTUM UINT64_C(4096)

enum {
  OPTION_SILENCE_DURATION = 256,
  OPTION_SILENCE_THRESHOLD,
};

struct cli_option {
  char *key;
  char *value;
  struct cli_option *next;
};

struct audio_sink {
  ao_device *device;
  int16_t *samples;
  size_t capacity;
};

static volatile sig_atomic_t interrupted;

static void handle_interrupt(int signal_number) {
  (void)signal_number;
  interrupted = 1;
}

static char *copy_string(const char *source, size_t length) {
  char *copy;

  if (length == SIZE_MAX) {
    return NULL;
  }
  copy = malloc(length + 1);
  if (copy == NULL) {
    return NULL;
  }
  memcpy(copy, source, length);
  copy[length] = '\0';
  return copy;
}

static void free_cli_options(struct cli_option *options) {
  while (options != NULL) {
    struct cli_option *next = options->next;
    free(options->key);
    free(options->value);
    free(options);
    options = next;
  }
}

static int append_cli_option(struct cli_option **options, const char *text) {
  const char *separator = strchr(text, '=');
  struct cli_option *option;

  if (separator == NULL || separator == text) {
    fprintf(stderr, "upse123: audio option must be KEY=VALUE: %s\n", text);
    return 0;
  }
  option = calloc(1, sizeof(*option));
  if (option == NULL) {
    return 0;
  }
  option->key = copy_string(text, (size_t)(separator - text));
  option->value = copy_string(separator + 1, strlen(separator + 1));
  if (option->key == NULL || option->value == NULL) {
    free_cli_options(option);
    return 0;
  }
  option->next = *options;
  *options = option;
  return 1;
}

static void usage(FILE *stream, const char *program) {
  fprintf(stream,
          "usage: %s [--driver NAME] [--ao-option KEY=VALUE] "
          "[--seek TIME] [--silence-duration MILLISECONDS] "
          "[--silence-threshold AMPLITUDE] FILE\n"
          "\n"
          "  -d, --driver NAME          select a libao live driver\n"
          "  -o, --ao-option KEY=VALUE pass an option to the libao driver\n"
          "  -s, --seek TIME           reset and fast-forward before playback\n"
          "      --silence-duration MS end after this many quiet milliseconds\n"
          "      --silence-threshold N maximum quiet normalized amplitude\n"
          "  -h, --help                show this help\n"
          "  -v, --version             show the player version\n",
          program);
}

static int parse_threshold(const char *text, float *output) {
  char *end = NULL;
  float value;

  errno = 0;
  value = strtof(text, &end);
  if (end == text || *end != '\0' || errno == ERANGE || !isfinite(value) ||
      value < 0.0f) {
    return 0;
  }
  *output = value;
  return 1;
}

static void report_upse_error(const char *operation, upse_result result,
                              upse_error **error) {
  const char *message = NULL;

  if (error != NULL && *error != NULL) {
    message = upse_error_message(*error);
  }
  if (message == NULL) {
    fprintf(stderr, "upse123: %s failed with result %" PRId32 "\n",
            operation, result);
  } else {
    fprintf(stderr, "upse123: %s: %s\n", operation, message);
  }
  if (error != NULL) {
    upse_error_free(*error);
    *error = NULL;
  }
}

static int parse_unsigned(const char *text, size_t length, uint64_t *output) {
  uint64_t value = 0;
  size_t index;

  if (length == 0) {
    return 0;
  }
  for (index = 0; index < length; ++index) {
    unsigned int digit;
    if (text[index] < '0' || text[index] > '9') {
      return 0;
    }
    digit = (unsigned int)(text[index] - '0');
    if (value > (UINT64_MAX - digit) / UINT64_C(10)) {
      return 0;
    }
    value = value * UINT64_C(10) + digit;
  }
  *output = value;
  return 1;
}

static int parse_seconds(const char *text, size_t length, uint64_t *whole,
                         uint64_t *fraction, uint64_t *scale) {
  const char *point = memchr(text, '.', length);
  size_t whole_length = point == NULL ? length : (size_t)(point - text);
  size_t fraction_length = point == NULL ? 0 : length - whole_length - 1;
  size_t index;

  if (!parse_unsigned(text, whole_length, whole)) {
    return 0;
  }
  *fraction = 0;
  *scale = 1;
  if (point == NULL) {
    return 1;
  }
  if (fraction_length == 0 || fraction_length > 14) {
    return 0;
  }
  for (index = 0; index < fraction_length; ++index) {
    unsigned int digit;
    char character = point[index + 1];
    if (character < '0' || character > '9') {
      return 0;
    }
    digit = (unsigned int)(character - '0');
    *fraction = *fraction * UINT64_C(10) + digit;
    *scale *= UINT64_C(10);
  }
  return 1;
}

static int seek_time_to_frames(const char *text, uint32_t sample_rate,
                               uint64_t *output) {
  const char *first = strchr(text, ':');
  const char *second = first == NULL ? NULL : strchr(first + 1, ':');
  const char *seconds_text;
  uint64_t hours = 0;
  uint64_t minutes = 0;
  uint64_t seconds;
  uint64_t fraction;
  uint64_t scale;
  uint64_t whole_seconds;
  uint64_t whole_frames;
  uint64_t fractional_product;
  uint64_t fractional_frames;
  size_t seconds_length;

  if (text[0] == '\0' || sample_rate == 0 ||
      (second != NULL && strchr(second + 1, ':') != NULL)) {
    return 0;
  }
  if (first == NULL) {
    seconds_text = text;
  } else if (second == NULL) {
    if (!parse_unsigned(text, (size_t)(first - text), &minutes)) {
      return 0;
    }
    seconds_text = first + 1;
  } else {
    if (!parse_unsigned(text, (size_t)(first - text), &hours) ||
        !parse_unsigned(first + 1, (size_t)(second - first - 1), &minutes) ||
        minutes >= UINT64_C(60)) {
      return 0;
    }
    seconds_text = second + 1;
  }
  seconds_length = strlen(seconds_text);
  if (!parse_seconds(seconds_text, seconds_length, &seconds, &fraction,
                     &scale) ||
      (first != NULL && seconds >= UINT64_C(60))) {
    return 0;
  }
  if (hours > UINT64_MAX / UINT64_C(3600)) {
    return 0;
  }
  whole_seconds = hours * UINT64_C(3600);
  if (minutes > (UINT64_MAX - whole_seconds) / UINT64_C(60)) {
    return 0;
  }
  whole_seconds += minutes * UINT64_C(60);
  if (seconds > UINT64_MAX - whole_seconds) {
    return 0;
  }
  whole_seconds += seconds;
  if (whole_seconds > UINT64_MAX / sample_rate) {
    return 0;
  }
  whole_frames = whole_seconds * sample_rate;
  if (fraction > UINT64_MAX / sample_rate) {
    return 0;
  }
  fractional_product = fraction * sample_rate;
  fractional_frames = fractional_product / scale;
  if (fractional_frames > UINT64_MAX - whole_frames) {
    return 0;
  }
  *output = whole_frames + fractional_frames;
  return 1;
}

static int16_t float_to_pcm16(float sample) {
  if (!isfinite(sample)) {
    return 0;
  }
  if (sample >= 1.0f) {
    return INT16_MAX;
  }
  if (sample <= -1.0f) {
    return INT16_MIN;
  }
  if (sample >= 0.0f) {
    return (int16_t)(sample * 32767.0f + 0.5f);
  }
  return (int16_t)(sample * 32768.0f - 0.5f);
}

static upse_audio_action discard_audio(void *userdata, const float *samples,
                                       size_t frames) {
  (void)userdata;
  (void)samples;
  (void)frames;
  return UPSE_CALLBACK_CONTINUE;
}

static upse_audio_action play_audio(void *userdata, const float *samples,
                                    size_t frames) {
  struct audio_sink *sink = userdata;
  size_t scalar_count;
  size_t byte_count;
  size_t index;

  if (sink == NULL || sink->device == NULL ||
      (samples == NULL && frames != 0) || frames > SIZE_MAX / 2) {
    return UPSE_CALLBACK_ERROR;
  }
  scalar_count = frames * 2;
  if (scalar_count > SIZE_MAX / sizeof(*sink->samples)) {
    return UPSE_CALLBACK_ERROR;
  }
  if (scalar_count > sink->capacity) {
    int16_t *resized = realloc(sink->samples,
                               scalar_count * sizeof(*sink->samples));
    if (resized == NULL) {
      return UPSE_CALLBACK_ERROR;
    }
    sink->samples = resized;
    sink->capacity = scalar_count;
  }
  for (index = 0; index < scalar_count; ++index) {
    sink->samples[index] = float_to_pcm16(samples[index]);
  }
  byte_count = scalar_count * sizeof(*sink->samples);
  if (byte_count > UINT32_MAX ||
      !ao_play(sink->device, (char *)sink->samples, (uint_32)byte_count)) {
    return UPSE_CALLBACK_ERROR;
  }
  return UPSE_CALLBACK_CONTINUE;
}

static int fast_forward(upse_player *player, uint64_t target) {
  upse_error *error = NULL;
  upse_result result;
  uint64_t remaining = target;

  result = upse_player_set_callback(player, discard_audio, NULL, &error);
  if (result != UPSE_RESULT_OK) {
    report_upse_error("install discard callback", result, &error);
    return 0;
  }
  result = upse_player_reset(player, &error);
  if (result != UPSE_RESULT_OK) {
    report_upse_error("reset before seek", result, &error);
    return 0;
  }
  while (remaining != 0) {
    uint64_t request = remaining < RENDER_QUANTUM ? remaining : RENDER_QUANTUM;
    upse_render_outcome outcome = {sizeof(outcome), 0, 0};

    result = upse_player_render(player, request, &outcome, &error);
    if (result != UPSE_RESULT_OK) {
      report_upse_error("fast-forward render", result, &error);
      return 0;
    }
    if (outcome.frames == 0 || outcome.frames > request ||
        outcome.frames > remaining) {
      fprintf(stderr, "upse123: invalid progress while fast-forwarding\n");
      return 0;
    }
    remaining -= outcome.frames;
    if (outcome.kind == UPSE_RENDER_STOPPED) {
      fprintf(stderr, "upse123: discard callback stopped unexpectedly\n");
      return 0;
    }
    if (outcome.kind == UPSE_RENDER_END && remaining != 0) {
      fprintf(stderr, "upse123: module ended before requested seek time\n");
      return 0;
    }
  }
  return 1;
}

static void print_field(const upse_player *player, upse_metadata_field field,
                        const char *label) {
  const char *value = upse_player_metadata(player, field);
  if (value != NULL) {
    printf("%-10s %s\n", label, value);
  }
}

static void print_metadata(const upse_player *player,
                           const upse_audio_format *format) {
  uint64_t frames;

  print_field(player, UPSE_METADATA_GAME, "Game:");
  print_field(player, UPSE_METADATA_TITLE, "Title:");
  print_field(player, UPSE_METADATA_ARTIST, "Artist:");
  print_field(player, UPSE_METADATA_YEAR, "Year:");
  print_field(player, UPSE_METADATA_GENRE, "Genre:");
  print_field(player, UPSE_METADATA_PSF_BY, "Ripper:");
  print_field(player, UPSE_METADATA_COPYRIGHT, "Copyright:");
  print_field(player, UPSE_METADATA_COMMENT, "Comment:");
  printf("Format:    %" PRIu32 " Hz, %" PRIu32 " channels, f32\n",
         format->sample_rate, format->channels);
  printf("Volume:    %.6g\n", upse_player_volume(player));
  if (upse_player_length_frames(player, &frames)) {
    printf("Length:    %" PRIu64 " frames\n", frames);
  } else {
    printf("Length:    unknown\n");
  }
  if (upse_player_fade_frames(player, &frames)) {
    printf("Fade:      %" PRIu64 " frames\n", frames);
  }
}

static const char *ao_error_message(int code) {
  switch (code) {
  case AO_ENODRIVER:
    return "no such driver";
  case AO_ENOTLIVE:
    return "driver is not a live-output driver";
  case AO_EBADOPTION:
    return "invalid driver option";
  case AO_EOPENDEVICE:
    return "cannot open audio device";
  case AO_EBADFORMAT:
    return "unsupported audio format";
  default:
    return "libao failure";
  }
}

int main(int argc, char **argv) {
  static const struct option long_options[] = {
      {"driver", required_argument, NULL, 'd'},
      {"ao-option", required_argument, NULL, 'o'},
      {"seek", required_argument, NULL, 's'},
      {"silence-duration", required_argument, NULL, OPTION_SILENCE_DURATION},
      {"silence-threshold", required_argument, NULL,
       OPTION_SILENCE_THRESHOLD},
      {"help", no_argument, NULL, 'h'},
      {"version", no_argument, NULL, 'v'},
      {NULL, 0, NULL, 0},
  };
  const char *driver_name = NULL;
  const char *seek_time = NULL;
  uint64_t trailing_silence_ms = 0;
  float silence_threshold = 0.0f;
  int silence_threshold_set = 0;
  struct cli_option *cli_options = NULL;
  struct cli_option *option;
  ao_option *ao_options = NULL;
  ao_device *device = NULL;
  struct audio_sink sink = {NULL, NULL, 0};
  upse_player *player = NULL;
  upse_error *error = NULL;
  upse_config config;
  upse_audio_format format = {sizeof(format), 0, 0};
  upse_result result;
  int driver_id;
  int option_character;
  int ao_initialized = 0;
  int exit_code = EXIT_FAILURE;

  while ((option_character =
              getopt_long(argc, argv, "d:o:s:hv", long_options, NULL)) != -1) {
    switch (option_character) {
    case 'd':
      driver_name = optarg;
      break;
    case 'o':
      if (!append_cli_option(&cli_options, optarg)) {
        goto cleanup;
      }
      break;
    case 's':
      seek_time = optarg;
      break;
    case OPTION_SILENCE_DURATION:
      if (!parse_unsigned(optarg, strlen(optarg), &trailing_silence_ms)) {
        fprintf(stderr, "upse123: invalid silence duration: %s\n", optarg);
        goto cleanup;
      }
      break;
    case OPTION_SILENCE_THRESHOLD:
      if (!parse_threshold(optarg, &silence_threshold)) {
        fprintf(stderr, "upse123: invalid silence threshold: %s\n", optarg);
        goto cleanup;
      }
      silence_threshold_set = 1;
      break;
    case 'h':
      usage(stdout, argv[0]);
      exit_code = EXIT_SUCCESS;
      goto cleanup;
    case 'v':
      printf("upse123 %s\n", UPSE123_VERSION);
      exit_code = EXIT_SUCCESS;
      goto cleanup;
    default:
      usage(stderr, argv[0]);
      goto cleanup;
    }
  }
  if (argc - optind != 1) {
    usage(stderr, argv[0]);
    goto cleanup;
  }
  if (signal(SIGINT, handle_interrupt) == SIG_ERR) {
    fprintf(stderr, "upse123: cannot install SIGINT handler\n");
    goto cleanup;
  }
  result = upse_config_init(&config);
  if (result != UPSE_RESULT_OK) {
    report_upse_error("initialize configuration", result, &error);
    goto cleanup;
  }
  config.trailing_silence_ms = trailing_silence_ms;
  if (silence_threshold_set) {
    config.silence_threshold = silence_threshold;
  }
  result = upse_player_open_path(argv[optind], &config, &player, &error);
  if (result != UPSE_RESULT_OK) {
    report_upse_error("open module", result, &error);
    goto cleanup;
  }
  result = upse_player_audio_format(player, &format, &error);
  if (result != UPSE_RESULT_OK) {
    report_upse_error("query audio format", result, &error);
    goto cleanup;
  }
  if (format.channels != 2 || format.sample_rate == 0 ||
      format.sample_rate > INT_MAX) {
    fprintf(stderr, "upse123: unsupported native audio format\n");
    goto cleanup;
  }
  print_metadata(player, &format);
  if (seek_time != NULL) {
    uint64_t seek_frames;
    if (!seek_time_to_frames(seek_time, format.sample_rate, &seek_frames)) {
      fprintf(stderr, "upse123: invalid or out-of-range seek time: %s\n",
              seek_time);
      goto cleanup;
    }
    if (!fast_forward(player, seek_frames)) {
      goto cleanup;
    }
    printf("Seek:      %" PRIu64 " frames\n", seek_frames);
  }

  ao_initialize();
  ao_initialized = 1;
  for (option = cli_options; option != NULL; option = option->next) {
    if (!ao_append_option(&ao_options, option->key, option->value)) {
      fprintf(stderr, "upse123: cannot allocate libao option\n");
      goto cleanup;
    }
  }
  driver_id = driver_name == NULL ? ao_default_driver_id()
                                  : ao_driver_id(driver_name);
  if (driver_id < 0) {
    fprintf(stderr, "upse123: unknown libao driver: %s\n",
            driver_name == NULL ? "(default)" : driver_name);
    goto cleanup;
  }
  {
    ao_sample_format ao_format = {16, (int)format.sample_rate, 2,
                                  AO_FMT_NATIVE, (char *)"L,R"};
    device = ao_open_live(driver_id, &ao_format, ao_options);
  }
  if (device == NULL) {
    fprintf(stderr, "upse123: cannot open audio output: %s\n",
            ao_error_message(errno));
    goto cleanup;
  }
  sink.device = device;
  result = upse_player_set_callback(player, play_audio, &sink, &error);
  if (result != UPSE_RESULT_OK) {
    report_upse_error("install audio callback", result, &error);
    goto cleanup;
  }

  while (!interrupted) {
    upse_render_outcome outcome = {sizeof(outcome), 0, 0};
    result = upse_player_render(player, RENDER_QUANTUM, &outcome, &error);
    if (result != UPSE_RESULT_OK) {
      report_upse_error("render", result, &error);
      goto cleanup;
    }
    if (outcome.kind == UPSE_RENDER_END) {
      break;
    }
    if (outcome.kind != UPSE_RENDER_COMPLETE || outcome.frames == 0) {
      fprintf(stderr, "upse123: playback stopped without reaching the end\n");
      goto cleanup;
    }
  }
  exit_code = EXIT_SUCCESS;

cleanup:
  if (player != NULL) {
    (void)upse_player_set_callback(player, NULL, NULL, NULL);
  }
  if (device != NULL) {
    (void)ao_close(device);
  }
  free(sink.samples);
  ao_free_options(ao_options);
  if (ao_initialized) {
    ao_shutdown();
  }
  upse_error_free(error);
  upse_player_free(player);
  free_cli_options(cli_options);
  return exit_code;
}
