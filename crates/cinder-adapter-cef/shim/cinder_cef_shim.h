/* Cinder L0 shim: a flat C surface over the CEF C API.
 *
 * This is the only file in the project that includes CEF headers. It exposes
 * plain functions and plain buffers so the Rust side never has to mirror CEF's
 * vtable layouts. Everything here is single-browser, single-threaded, and
 * deliberately small enough to audit in one sitting.
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

/* Initialize CEF. Returns 1 on success, 0 on failure. */
int cinder_cef_init(int argc, char** argv, const char* resources_dir,
                    const char* locales_dir, const char* cache_dir);

/* Create one windowless (off-screen) browser. Returns 1 on success. */
int cinder_cef_create_browser(const char* url, int width, int height);

/* Run the CEF message loop for up to `ms` milliseconds. */
void cinder_cef_pump(int ms);

/* State bit flags returned by cinder_cef_state(). */
#define CINDER_CEF_CREATED 1
#define CINDER_CEF_LOADED 2
#define CINDER_CEF_PAINTED 4
#define CINDER_CEF_CLOSED 8

int cinder_cef_state(void);

/* Size of the last painted frame. Returns 1 if a frame is available. */
int cinder_cef_frame_size(int* width, int* height);

/* Copy the last painted frame as BGRA into dst. Returns bytes written, or 0. */
int cinder_cef_copy_frame(unsigned char* dst, int capacity);

/* UTF-8 views of the current title and URL. Valid until the next pump. */
const char* cinder_cef_title(void);
const char* cinder_cef_url(void);

/* CEF/Chromium version string, e.g. "158.0.1+g6ee3fb9+chromium-158.0.8127.18". */
const char* cinder_cef_version(void);

/* Last shim-level error, or "" if none. */
const char* cinder_cef_last_error(void);

void cinder_cef_shutdown(void);

#ifdef __cplusplus
}
#endif

#endif /* CINDER_CEF_SHIM_H */
