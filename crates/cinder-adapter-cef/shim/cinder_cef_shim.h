/* Cinder L0 shim: a flat C surface over the CEF C API.
 *
 * This is the only file in the project that includes CEF headers. It exposes
 * plain functions and plain buffers so the Rust side never has to mirror CEF's
 * vtable layouts. Everything here is single-threaded: every function must be
 * called on the thread that called cinder_cef_init(), because that is the
 * thread CEF runs its message loop on.
 *
 * Views are addressed by a caller-supplied integer id. The shim never invents
 * ids and never reuses one: a closed view's slot stays closed until
 * cinder_cef_shutdown().
 */
#ifndef CINDER_CEF_SHIM_H
#define CINDER_CEF_SHIM_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Must be called first, before cinder_cef_init. Returns >= 0 when this process
 * is a CEF subprocess and should exit with that code immediately. */
int cinder_cef_execute_process(int argc, char** argv);

/* Initialize CEF. Returns 1 on success, 0 on failure. `windowless` enables
 * off-screen rendering for the whole process; a windowed shell passes 0. */
int cinder_cef_init(int argc, char** argv, const char* resources_dir,
                    const char* locales_dir, const char* cache_dir);
int cinder_cef_init_ex(int argc, char** argv, const char* resources_dir,
                       const char* locales_dir, const char* cache_dir,
                       int windowless, int external_pump);

/* ------------------------------------------------------------- events --- */

/* Event kinds passed to the callback. `a` and `b` are UTF-8 and are only valid
 * for the duration of the call; copy anything you keep. */
#define CINDER_CEF_EV_CREATED 1          /* a = url                              */
#define CINDER_CEF_EV_LOAD_START 2       /* a = url                              */
#define CINDER_CEF_EV_LOAD_END 3         /* a = url                              */
#define CINDER_CEF_EV_LOAD_ERROR 4       /* a = decimal error code, b = failed url */
#define CINDER_CEF_EV_TITLE 5            /* a = title                            */
#define CINDER_CEF_EV_URL 6              /* a = url                              */
#define CINDER_CEF_EV_CLOSED 7           /* -                                    */
#define CINDER_CEF_EV_CLOSE_REQUESTED 8  /* -                                    */
#define CINDER_CEF_EV_PAINTED 9          /* -                                    */
#define CINDER_CEF_EV_BLOCKED 10         /* a = url cancelled by policy          */

typedef void (*cinder_cef_event_fn)(void* user_data, int view_id, int kind,
                                    const char* a, const char* b);

/* Register the event sink. Called from CEF callbacks on the message-loop
 * thread. Passing NULL unregisters. */
void cinder_cef_set_event_callback(cinder_cef_event_fn cb, void* user_data);

/* -------------------------------------------------------------- views --- */

/* Create one browser under `view_id`. `parent` is the shell's native window
 * handle: zero creates a windowless (off-screen) browser, non-zero embeds a
 * child window in that parent. Returns 1 on success. Fails if the id is already
 * used or the table is full. */
int cinder_cef_create_view(int view_id, const char* url, int width, int height,
                           unsigned long long parent, int top);

/* Ask a view to close. `force` skips page-level beforeunload handlers.
 * Returns 1 if the request was issued. */
int cinder_cef_close_view(int view_id, int force);

int cinder_cef_navigate(int view_id, const char* url);
int cinder_cef_reload(int view_id);
int cinder_cef_stop(int view_id);
int cinder_cef_go_back(int view_id);
int cinder_cef_go_forward(int view_id);
int cinder_cef_can_go_back(int view_id);
int cinder_cef_can_go_forward(int view_id);
int cinder_cef_set_zoom(int view_id, double level);
int cinder_cef_set_mute(int view_id, int muted);
int cinder_cef_find(int view_id, const char* query, int forward);
int cinder_cef_resize(int view_id, int width, int height);
int cinder_cef_set_visible(int view_id, int visible);
int cinder_cef_set_focus(int view_id, int focused);

/* State bit flags returned by cinder_cef_view_state(). */
#define CINDER_CEF_CREATED 1
#define CINDER_CEF_LOADED 2
#define CINDER_CEF_PAINTED 4
#define CINDER_CEF_CLOSED 8

/* 0 for an unknown or already-released view. */
int cinder_cef_view_state(int view_id);

/* --------------------------------------------------------------- input --- */
/* Windowless rendering hides OS events from CEF, so the shell forwards them.
 * Coordinates are view-relative DIPs; `modifiers` uses cef_event_flags_t;
 * `button` uses cef_mouse_button_type_t; `type` uses cef_key_event_type_t.
 * Every call is a no-op for an unknown view. */

void cinder_cef_send_mouse_move(int view_id, int x, int y, unsigned int modifiers,
                                int leave);
void cinder_cef_send_mouse_click(int view_id, int x, int y,
                                 unsigned int modifiers, int button, int up,
                                 int click_count);
void cinder_cef_send_mouse_wheel(int view_id, int x, int y,
                                 unsigned int modifiers, int delta_x,
                                 int delta_y);
void cinder_cef_send_key(int view_id, int type, unsigned int modifiers,
                         int windows_key_code, int native_key_code,
                         int is_system_key, unsigned int character,
                         unsigned int unmodified_character);

/* Size of the last painted frame. Returns 1 if a frame is available. */
int cinder_cef_view_frame_size(int view_id, int* width, int* height);

/* Copy the last painted frame as BGRA into dst. Returns bytes written, or 0. */
int cinder_cef_copy_view_frame(int view_id, unsigned char* dst, int capacity);

/* UTF-8 views of the current title and URL. Valid until the next pump. */
const char* cinder_cef_view_title(int view_id);
const char* cinder_cef_view_url(int view_id);

/* ------------------------------------------------------------- policy --- */

/* Install the request policy consulted in on_before_resource_load.
 * `blocked_hosts` is an array of `count` NUL-terminated hostnames; a host also
 * matches its subdomains. `upgrade_https` rewrites http:// to https://.
 * Returns 1 on success. The strings are copied. */
int cinder_cef_set_policy(const char* const* blocked_hosts, int count,
                          int upgrade_https);

/* -------------------------------------------------------------- misc ---- */

/* Run the CEF message loop for up to `ms` milliseconds. */
void cinder_cef_pump(int ms);

/* CEF/Chromium version string, e.g. "158.0.1+g6ee3fb9+chromium-158.0.8127.18". */
const char* cinder_cef_version(void);

/* Last shim-level error, or "" if none. */
const char* cinder_cef_last_error(void);

void cinder_cef_shutdown(void);

/* ------------------------------------------- single-view compatibility --- */
/* The spike uses these. They address the first view created through
 * cinder_cef_create_view() or cinder_cef_create_browser(). */

int cinder_cef_create_browser(const char* url, int width, int height);
int cinder_cef_state(void);
int cinder_cef_frame_size(int* width, int* height);
int cinder_cef_copy_frame(unsigned char* dst, int capacity);
const char* cinder_cef_title(void);
const char* cinder_cef_url(void);

#ifdef __cplusplus
}
#endif

#endif /* CINDER_CEF_SHIM_H */
