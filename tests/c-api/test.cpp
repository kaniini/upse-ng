// SPDX-License-Identifier: LGPL-2.1-or-later

#include <cstdint>
#include <cstdlib>

#include <upse.h>
#include <upse.h>

static upse_audio_action consume(void *, const float *samples,
                                 std::size_t frames) {
  return samples == nullptr && frames != 0 ? UPSE_CALLBACK_ERROR
                                           : UPSE_CALLBACK_CONTINUE;
}

int main(int argc, char **argv) {
  static_assert(sizeof(upse_result) == sizeof(std::int32_t));
  static_assert(sizeof(upse_audio_action) == sizeof(std::int32_t));
  if (argc != 2 || upse_abi_version() != UPSE_ABI_VERSION) {
    return EXIT_FAILURE;
  }
  upse_config config{};
  upse_player *player = nullptr;
  upse_error *error = nullptr;
  if (upse_config_init(&config) != UPSE_RESULT_OK ||
      upse_player_open_path(argv[1], &config, &player, &error) !=
          UPSE_RESULT_OK ||
      upse_player_set_callback(player, consume, nullptr, &error) !=
          UPSE_RESULT_OK) {
    upse_error_free(error);
    upse_player_free(player);
    return EXIT_FAILURE;
  }
  upse_render_outcome outcome{sizeof(outcome), 0, 0};
  const upse_result result = upse_player_render(player, 8, &outcome, &error);
  upse_error_free(error);
  upse_player_free(player);
  return result == UPSE_RESULT_OK && outcome.frames == 8 ? EXIT_SUCCESS
                                                         : EXIT_FAILURE;
}
