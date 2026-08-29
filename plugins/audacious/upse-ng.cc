// SPDX-License-Identifier: LGPL-2.1-or-later

#include <libaudcore/audstrings.h>
#include <libaudcore/plugin.h>
#include <libaudcore/preferences.h>
#include <libaudcore/runtime.h>

#include <upse.h>

#include <climits>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <memory>
#include <new>
#include <string>
#include <utility>

namespace
{

constexpr uint64_t RenderQuantum = 4096;
constexpr uint64_t SeekQuantum = 65536;
constexpr const char * ConfigSection = "upse-ng";

const char * const ConfigDefaults[] = {
    "detect_silence", "FALSE",
    "silence_duration", "5000",
    "silence_threshold", "0.000030517578125",
    nullptr,
};

const PreferencesWidget PreferenceWidgets[] = {
    WidgetLabel("<b>Silence detection</b>"),
    WidgetCheck("Stop playback after trailing silence",
                WidgetBool(ConfigSection, "detect_silence")),
    WidgetSpin("Quiet duration:",
               WidgetInt(ConfigSection, "silence_duration"),
               {100, 60000, 100, "ms"}),
    WidgetSpin("Quiet threshold:",
               WidgetFloat(ConfigSection, "silence_threshold"),
               {0.0, 1.0, 0.00001, "normalized amplitude"}),
};

const PluginPreferences Preferences = {
    {PreferenceWidgets},
    nullptr,
    nullptr,
    nullptr,
};

const char * const extensions[] = {"psf", "minipsf", "psf2", "minipsf2",
                                   nullptr};

using PlayerPtr = std::unique_ptr<upse_player, decltype(&upse_player_free)>;

struct ResolverBlob
{
    ResolverBlob(Index<char> && bytes, std::string && resolved_origin)
        : data(std::move(bytes)), origin(std::move(resolved_origin))
    {
    }

    Index<char> data;
    std::string origin;
};

void release_resolved_blob(void * userdata, const uint8_t *, size_t)
{
    delete static_cast<ResolverBlob *>(userdata);
}

upse_result resolve_dependency(void *, const char * containing_origin,
                               const char * reference, upse_blob * output)
{
    if (!containing_origin || !reference || !output)
        return UPSE_RESULT_INVALID_ARGUMENT;

    StringBuf uri = uri_construct(reference, containing_origin);
    const char * uri_text = uri;
    if (!uri_text)
        return UPSE_RESULT_IO;

    std::string origin(uri_text);
    VFSFile file(origin.c_str(), "r");
    if (!file)
        return UPSE_RESULT_IO;

    Index<char> bytes = file.read_all();
    if (bytes.len() == 0)
        return UPSE_RESULT_IO;

    auto * blob = new (std::nothrow)
        ResolverBlob(std::move(bytes), std::move(origin));
    if (!blob)
        return UPSE_RESULT_INTERNAL;

    *output = {
        sizeof(*output),
        reinterpret_cast<const uint8_t *>(blob->data.begin()),
        static_cast<size_t>(blob->data.len()),
        blob->origin.c_str(),
        blob,
        release_resolved_blob,
    };
    return UPSE_RESULT_OK;
}

void report_error(const char * operation, upse_result result,
                  upse_error * error)
{
    const char * message = error ? upse_error_message(error) : nullptr;
    if (message)
        AUDERR("upse-ng: %s: %s\n", operation, message);
    else
        AUDERR("upse-ng: %s failed with result %d\n", operation,
               static_cast<int>(result));
    upse_error_free(error);
}

bool read_root(VFSFile & file, Index<char> & bytes)
{
    if (!file || file.fseek(0, VFS_SEEK_SET) != 0)
        return false;
    bytes = file.read_all();
    return bytes.len() >= 4;
}

PlayerPtr open_player(const char * filename, const Index<char> & bytes,
                      bool playback)
{
    PlayerPtr player(nullptr, upse_player_free);
    upse_config config{};
    upse_result result = upse_config_init(&config);
    if (result != UPSE_RESULT_OK)
    {
        report_error("initialize configuration", result, nullptr);
        return player;
    }

    if (playback && aud_get_bool(ConfigSection, "detect_silence"))
    {
        const int duration = aud_get_int(ConfigSection, "silence_duration");
        const double threshold =
            aud_get_double(ConfigSection, "silence_threshold");
        if (duration > 0 && threshold >= 0.0 && threshold <= 1.0)
        {
            config.trailing_silence_ms = static_cast<uint64_t>(duration);
            config.silence_threshold = static_cast<float>(threshold);
        }
        else
        {
            AUDERR("upse-ng: ignoring invalid silence detection settings\n");
        }
    }

    upse_resolver resolver = {
        sizeof(resolver),
        nullptr,
        resolve_dependency,
    };
    upse_player * raw_player = nullptr;
    upse_error * error = nullptr;
    result = upse_player_open_memory(
        reinterpret_cast<const uint8_t *>(bytes.begin()),
        static_cast<size_t>(bytes.len()), filename, &config, &resolver,
        &raw_player, &error);
    if (result != UPSE_RESULT_OK)
    {
        report_error("open module", result, error);
        return player;
    }
    return PlayerPtr(raw_player, upse_player_free);
}

bool query_format(upse_player * player, upse_audio_format & format)
{
    format = {sizeof(format), 0, 0};
    upse_error * error = nullptr;
    const upse_result result =
        upse_player_audio_format(player, &format, &error);
    if (result != UPSE_RESULT_OK)
    {
        report_error("query audio format", result, error);
        return false;
    }
    if (format.channels != 2 || format.sample_rate == 0 ||
        format.sample_rate > static_cast<uint32_t>(INT_MAX))
    {
        AUDERR("upse-ng: unsupported audio format: %u Hz, %u channels\n",
               format.sample_rate, format.channels);
        return false;
    }
    return true;
}

void set_metadata(Tuple & tuple, upse_player * player,
                  upse_metadata_field source, Tuple::Field destination)
{
    const char * value = upse_player_metadata(player, source);
    if (value)
        tuple.set_str(destination, value);
}

int year_number(const char * value)
{
    if (!value || std::strlen(value) < 4)
        return -1;
    int year = 0;
    for (int index = 0; index < 4; ++index)
    {
        const char digit = value[index];
        if (digit < '0' || digit > '9')
            return -1;
        year = year * 10 + digit - '0';
    }
    return year;
}

int frames_to_milliseconds(uint64_t frames, uint32_t sample_rate)
{
    const uint64_t seconds = frames / sample_rate;
    if (seconds > static_cast<uint64_t>(INT_MAX / 1000))
        return INT_MAX;
    const uint64_t remainder = frames % sample_rate;
    const uint64_t milliseconds =
        seconds * 1000 +
        (remainder * 1000 + static_cast<uint64_t>(sample_rate) / 2) /
            sample_rate;
    return milliseconds > static_cast<uint64_t>(INT_MAX)
               ? INT_MAX
               : static_cast<int>(milliseconds);
}

const char * format_name(const Index<char> & bytes)
{
    if (bytes.len() >= 4 && bytes[3] == 2)
        return "PlayStation 2 Sound Format (PSF2)";
    return "PlayStation Sound Format (PSF)";
}

void fill_tuple(Tuple & tuple, upse_player * player,
                const upse_audio_format & format, const Index<char> & bytes)
{
    set_metadata(tuple, player, UPSE_METADATA_TITLE, Tuple::Title);
    set_metadata(tuple, player, UPSE_METADATA_ARTIST, Tuple::Artist);
    set_metadata(tuple, player, UPSE_METADATA_GAME, Tuple::Album);
    set_metadata(tuple, player, UPSE_METADATA_GENRE, Tuple::Genre);
    set_metadata(tuple, player, UPSE_METADATA_COMMENT, Tuple::Comment);
    set_metadata(tuple, player, UPSE_METADATA_COPYRIGHT, Tuple::Copyright);
    set_metadata(tuple, player, UPSE_METADATA_PSF_BY, Tuple::Description);

    if (const char * year =
            upse_player_metadata(player, UPSE_METADATA_YEAR))
    {
        tuple.set_str(Tuple::Date, year);
        const int numeric_year = year_number(year);
        if (numeric_year >= 0)
            tuple.set_int(Tuple::Year, numeric_year);
    }

    uint64_t length = 0;
    if (upse_player_length_frames(player, &length))
    {
        uint64_t fade = 0;
        if (upse_player_fade_frames(player, &fade))
            length = UINT64_MAX - length < fade ? UINT64_MAX : length + fade;
        tuple.set_int(Tuple::Length,
                      frames_to_milliseconds(length, format.sample_rate));
    }

    tuple.set_format(format_name(bytes), static_cast<int>(format.channels),
                     static_cast<int>(format.sample_rate), 0);
    tuple.set_state(Tuple::Valid);
}

} // namespace

class UpseInputPlugin final : public InputPlugin
{
public:
    constexpr UpseInputPlugin()
        : InputPlugin(
              {
                  "UPSE-NG PSF Decoder",
                  nullptr,
                  "PlayStation and PlayStation 2 Sound Format decoder using "
                  "libupse-ng",
                  &Preferences,
                  0,
              },
              InputInfo().with_exts(extensions).with_priority(1))
    {
    }

    bool init() override
    {
        aud_config_set_defaults(ConfigSection, ConfigDefaults);
        return true;
    }

    bool is_our_file(const char *, VFSFile & file) override
    {
        const int64_t position = file.ftell();
        uint8_t header[4] = {};
        const int64_t read = file.fread(header, 1, sizeof(header));
        if (position >= 0 && file.fseek(position, VFS_SEEK_SET) != 0)
            return false;
        return read == static_cast<int64_t>(sizeof(header)) &&
               std::memcmp(header, "PSF", 3) == 0 &&
               (header[3] == 1 || header[3] == 2);
    }

    bool read_tag(const char * filename, VFSFile & file, Tuple & tuple,
                  Index<char> * image) override
    {
        (void)image;
        Index<char> bytes;
        if (!read_root(file, bytes))
        {
            AUDERR("upse-ng: cannot read %s\n", filename);
            return false;
        }
        PlayerPtr player = open_player(filename, bytes, false);
        if (!player)
            return false;
        upse_audio_format format{};
        if (!query_format(player.get(), format))
            return false;
        fill_tuple(tuple, player.get(), format, bytes);
        return true;
    }

    bool play(const char * filename, VFSFile & file) override
    {
        Index<char> bytes;
        if (!read_root(file, bytes))
        {
            AUDERR("upse-ng: cannot read %s\n", filename);
            return false;
        }
        PlayerPtr player = open_player(filename, bytes, true);
        if (!player)
            return false;

        upse_audio_format format{};
        if (!query_format(player.get(), format))
            return false;

        Tuple tuple;
        tuple.set_filename(filename);
        fill_tuple(tuple, player.get(), format, bytes);
        set_playback_tuple(std::move(tuple));

        if (!bind_audio_callback(player.get()))
            return false;
        open_audio(FMT_FLOAT, static_cast<int>(format.sample_rate),
                   static_cast<int>(format.channels));

        while (!check_stop())
        {
            const int seek = check_seek();
            if (seek >= 0)
            {
                bool at_end = false;
                if (!seek_to(player.get(), format.sample_rate, seek, at_end))
                    return false;
                if (at_end)
                    return true;
            }

            upse_render_outcome outcome = {sizeof(outcome), 0, 0};
            upse_error * error = nullptr;
            const upse_result result = upse_player_render(
                player.get(), RenderQuantum, &outcome, &error);
            if (result != UPSE_RESULT_OK)
            {
                report_error("render", result, error);
                return false;
            }
            if (outcome.kind == UPSE_RENDER_END ||
                outcome.kind == UPSE_RENDER_STOPPED)
                return true;
            if (outcome.frames == 0)
            {
                AUDERR("upse-ng: renderer made no progress\n");
                return false;
            }
        }
        return true;
    }

private:
    static upse_audio_action audio_callback(void * userdata,
                                            const float * samples,
                                            size_t frames)
    {
        auto * self = static_cast<UpseInputPlugin *>(userdata);
        if (!self || (!samples && frames != 0) ||
            frames > static_cast<size_t>(INT_MAX) /
                         (2 * sizeof(*samples)))
            return UPSE_CALLBACK_ERROR;
        if (check_stop())
            return UPSE_CALLBACK_STOP;
        write_audio(samples,
                    static_cast<int>(frames * 2 * sizeof(*samples)));
        return UPSE_CALLBACK_CONTINUE;
    }

    bool bind_audio_callback(upse_player * player)
    {
        upse_error * error = nullptr;
        const upse_result result = upse_player_set_callback(
            player, audio_callback, this, &error);
        if (result != UPSE_RESULT_OK)
        {
            report_error("install audio callback", result, error);
            return false;
        }
        return true;
    }

    bool seek_to(upse_player * player, uint32_t sample_rate,
                 int milliseconds, bool & at_end)
    {
        at_end = false;
        upse_error * error = nullptr;
        upse_result result = upse_player_reset(player, &error);
        if (result != UPSE_RESULT_OK)
        {
            report_error("reset before seek", result, error);
            return false;
        }

        const uint64_t target =
            static_cast<uint64_t>(milliseconds) * sample_rate / 1000;
        uint64_t remaining = target;
        while (remaining != 0 && !check_stop())
        {
            const uint64_t request =
                remaining < SeekQuantum ? remaining : SeekQuantum;
            upse_render_outcome outcome = {sizeof(outcome), 0, 0};
            error = nullptr;
            result = upse_player_advance(player, request, &outcome, &error);
            if (result != UPSE_RESULT_OK)
            {
                report_error("seek", result, error);
                return false;
            }
            if (outcome.frames == 0 || outcome.frames > remaining)
            {
                AUDERR("upse-ng: invalid progress while seeking\n");
                return false;
            }
            remaining -= outcome.frames;
            if (outcome.kind == UPSE_RENDER_END)
            {
                at_end = true;
                break;
            }
        }
        return true;
    }
};

extern "C"
{
__attribute__((visibility("default")))
UpseInputPlugin aud_plugin_instance;
}
