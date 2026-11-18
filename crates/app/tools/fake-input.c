/* Pointer and keyboard for a headless run under KWin: reads commands from standard input and sends
 * them through KWin's fake-input protocol, so that a run can click what a person would click.
 *
 *   move X Y          the pointer to X,Y (logical pixels of the output)
 *   click [X Y]       the left button, at X,Y when given
 *   right [X Y]       the right button
 *   press / release   the left button alone (for a drag: press, move..., release)
 *   scroll N          N wheel steps, positive is down
 *   key CODE          a key by its Linux code (1 Esc, 28 Enter, 63 F5, 57 Space ...)
 *   sleep SECONDS     wait
 *   # ...             a comment
 *
 * It keeps its connection until its input ends: KWin's pointer exists only as long as this program
 * does, and a window loses its open menus when the pointer goes away. KWin allows the protocol only
 * with KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 in the compositor's environment, which `headless.sh`
 * passes on. Build with tools/fake-input.sh, see crates/app/README.md.
 */
#define _POSIX_C_SOURCE 200809L
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <wayland-client.h>

#include "fake-input-client.h"

#define BTN_LEFT 0x110
#define BTN_RIGHT 0x111
#define WHEEL_STEP 15.0

static struct org_kde_kwin_fake_input *input;
static struct wl_display *display;

static void global(void *data, struct wl_registry *registry, uint32_t name, const char *interface, uint32_t version) {
    (void)data;
    if (strcmp(interface, org_kde_kwin_fake_input_interface.name) == 0)
        input = wl_registry_bind(registry, name, &org_kde_kwin_fake_input_interface, version < 4 ? version : 4);
}

static void global_remove(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data, (void)registry, (void)name;
}

static const struct wl_registry_listener listener = {global, global_remove};

static void pause_for(double seconds) {
    struct timespec wait = {(time_t)seconds, (long)((seconds - (time_t)seconds) * 1e9)};
    wl_display_flush(display);
    nanosleep(&wait, NULL);
}

static void move_to(double x, double y) {
    org_kde_kwin_fake_input_pointer_motion_absolute(input, wl_fixed_from_double(x), wl_fixed_from_double(y));
    wl_display_roundtrip(display);
    pause_for(0.05);
}

static void button(uint32_t code, uint32_t state) {
    org_kde_kwin_fake_input_button(input, code, state);
    wl_display_roundtrip(display);
    pause_for(0.05);
}

int main(void) {
    display = wl_display_connect(NULL);
    if (!display) {
        fprintf(stderr, "fake-input: no Wayland display\n");
        return 1;
    }
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &listener, NULL);
    wl_display_roundtrip(display);
    if (!input) {
        fprintf(stderr, "fake-input: the compositor offers no fake input (KWin with KWIN_WAYLAND_NO_PERMISSION_CHECKS=1?)\n");
        return 1;
    }
    org_kde_kwin_fake_input_authenticate(input, "am-ui headless run", "clicks of a test");
    wl_display_roundtrip(display);

    char line[256];
    while (fgets(line, sizeof line, stdin)) {
        char word[32] = "";
        double a = 0, b = 0;
        int count = sscanf(line, "%31s %lf %lf", word, &a, &b);
        if (count < 1 || word[0] == '#')
            continue;
        if (strcmp(word, "move") == 0 && count == 3) {
            move_to(a, b);
        } else if (strcmp(word, "click") == 0 || strcmp(word, "right") == 0) {
            uint32_t code = word[0] == 'c' ? BTN_LEFT : BTN_RIGHT;
            if (count == 3)
                move_to(a, b);
            button(code, 1);
            button(code, 0);
        } else if (strcmp(word, "press") == 0) {
            button(BTN_LEFT, 1);
        } else if (strcmp(word, "release") == 0) {
            button(BTN_LEFT, 0);
        } else if (strcmp(word, "scroll") == 0 && count >= 2) {
            org_kde_kwin_fake_input_axis(input, WL_POINTER_AXIS_VERTICAL_SCROLL, wl_fixed_from_double(a * WHEEL_STEP));
            wl_display_roundtrip(display);
        } else if (strcmp(word, "key") == 0 && count >= 2) {
            org_kde_kwin_fake_input_keyboard_key(input, (uint32_t)a, 1);
            org_kde_kwin_fake_input_keyboard_key(input, (uint32_t)a, 0);
            wl_display_roundtrip(display);
        } else if (strcmp(word, "sleep") == 0 && count >= 2) {
            pause_for(a);
        } else {
            fprintf(stderr, "fake-input: not understood: %s", line);
            return 2;
        }
    }
    wl_display_roundtrip(display);
    return 0;
}
