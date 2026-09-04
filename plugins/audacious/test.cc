// SPDX-License-Identifier: LGPL-2.1-or-later

#include <libaudcore/audstrings.h>
#include <libaudcore/plugin.h>
#include <libaudcore/runtime.h>

#include <upse.h>

#include <dlfcn.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>

namespace
{

bool has_extension(const InputPlugin * plugin, const char * expected)
{
    const char * const * extensions =
        plugin->input_info.keys[InputKey::Ext];
    if (!extensions)
        return false;
    for (; *extensions; ++extensions)
    {
        if (std::strcmp(*extensions, expected) == 0)
            return true;
    }
    return false;
}

bool string_field_equals(const Tuple & tuple, Tuple::Field field,
                         const char * expected)
{
    const String value = tuple.get_str(field);
    return value && std::strcmp(value, expected) == 0;
}

bool check_fixture(InputPlugin * plugin, const char * path,
                   const char * title, const char * codec,
                   int expected_length)
{
    StringBuf uri = filename_to_uri(path);
    const char * uri_text = uri;
    if (!uri_text)
    {
        std::fprintf(stderr, "cannot construct fixture URI: %s\n", path);
        return false;
    }

    VFSFile probe_file(uri_text, "r");
    if (!probe_file || !plugin->is_our_file(uri_text, probe_file))
    {
        std::fprintf(stderr, "plugin rejected fixture: %s\n", path);
        return false;
    }

    VFSFile tag_file(uri_text, "r");
    Tuple tuple;
    tuple.set_filename(uri_text);
    if (!tag_file || !plugin->read_tag(uri_text, tag_file, tuple, nullptr))
    {
        std::fprintf(stderr, "cannot read fixture metadata: %s\n", path);
        return false;
    }
    if (!tuple.valid() ||
        !string_field_equals(tuple, Tuple::Title, title) ||
        !string_field_equals(tuple, Tuple::Album, "Generated fixture") ||
        !string_field_equals(tuple, Tuple::Artist, "UPSE-NG tests") ||
        !string_field_equals(tuple, Tuple::Codec, codec) ||
        tuple.get_int(Tuple::Length) != expected_length ||
        tuple.get_int(Tuple::Channels) != 2)
    {
        std::fprintf(stderr, "unexpected fixture metadata: %s\n", path);
        return false;
    }
    return true;
}

} // namespace

int main(int argc, char ** argv)
{
    if (argc != 5)
    {
        std::fprintf(stderr,
                     "usage: %s PLUGIN PSF MINIPSF PSF2\n", argv[0]);
        return EXIT_FAILURE;
    }

    aud_set_headless_mode(true);
    aud_init();

    void * module = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (!module)
    {
        std::fprintf(stderr, "cannot load plugin: %s\n", dlerror());
        aud_cleanup();
        return EXIT_FAILURE;
    }

    auto * plugin = static_cast<InputPlugin *>(
        dlsym(module, "aud_plugin_instance"));
    const bool valid_plugin =
        plugin && plugin->magic == _AUD_PLUGIN_MAGIC &&
        plugin->version == _AUD_PLUGIN_VERSION &&
        plugin->type == PluginType::Input &&
        plugin->info.prefs &&
        has_extension(plugin, "psf") && has_extension(plugin, "minipsf") &&
        has_extension(plugin, "psf2") && has_extension(plugin, "minipsf2");
    if (!valid_plugin)
        std::fprintf(stderr, "invalid Audacious input plugin descriptor\n");

    bool valid_fixtures =
        valid_plugin && plugin->init() &&
        check_fixture(plugin, argv[2], "UPSE-NG synthetic noise",
                      "PlayStation Sound Format (PSF)", 25) &&
        check_fixture(plugin, argv[3], "UPSE-NG synthetic noise",
                      "PlayStation Sound Format (PSF)", 25) &&
        check_fixture(plugin, argv[4], "UPSE-NG synthetic PSF2",
                      "PlayStation 2 Sound Format (PSF2)", 25);

    if (valid_fixtures)
    {
        aud_set_int("upse-ng", "psf1_length_policy",
                    UPSE_DURATION_OVERRIDE);
        aud_set_int("upse-ng", "psf1_length_seconds", 1);
        aud_set_int("upse-ng", "psf1_fade_policy",
                    UPSE_DURATION_IGNORE);
        aud_set_int("upse-ng", "psf2_length_policy",
                    UPSE_DURATION_OVERRIDE);
        aud_set_int("upse-ng", "psf2_length_seconds", 2);
        aud_set_int("upse-ng", "psf2_fade_policy",
                    UPSE_DURATION_IGNORE);

        valid_fixtures =
            check_fixture(plugin, argv[2], "UPSE-NG synthetic noise",
                          "PlayStation Sound Format (PSF)", 1000) &&
            check_fixture(plugin, argv[4], "UPSE-NG synthetic PSF2",
                          "PlayStation 2 Sound Format (PSF2)", 2000);
    }

    if (valid_plugin)
        plugin->cleanup();
    dlclose(module);
    aud_cleanup();
    return valid_fixtures ? EXIT_SUCCESS : EXIT_FAILURE;
}
