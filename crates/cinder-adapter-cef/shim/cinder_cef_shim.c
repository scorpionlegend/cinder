/* Cinder L0 shim: flat C surface over the CEF C API. See cinder_cef_shim.h.
 *
 * Design notes:
 *  - Only the C API is used, so libcef_dll_wrapper (C++) is not needed at all.
 *  - Every handler is a static struct with a plain refcount; CEF add_refs them.
 *  - One browser, one frame buffer, one thread. No callbacks into Rust.
 */
#include "cinder_cef_shim.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "include/capi/cef_app_capi.h"
#include "include/capi/cef_browser_capi.h"
#include "include/capi/cef_client_capi.h"
#include "include/capi/cef_display_handler_capi.h"
#include "include/capi/cef_life_span_handler_capi.h"
#include "include/capi/cef_load_handler_capi.h"
#include "include/capi/cef_render_handler_capi.h"
#include "include/cef_api_hash.h"
#include "include/cef_version.h"
#include "include/internal/cef_string.h"
#include "include/internal/cef_types.h"

/* ---------------------------------------------------------------- state --- */

static int g_state = 0;
static int g_width = 0;
static int g_height = 0;
static unsigned char* g_frame = NULL;
static size_t g_frame_cap = 0;
static char g_title[512] = {0};
static char g_url[2048] = {0};
static char g_error[512] = {0};
static cef_browser_t* g_browser = NULL;

static void set_error(const char* msg) {
  snprintf(g_error, sizeof(g_error), "%s", msg ? msg : "");
}

/* ------------------------------------------------------------ refcounts --- */

#define DEFINE_REFCOUNT(prefix, counter)                                     \
  static void CEF_CALLBACK prefix##_add_ref(                                 \
      struct _cef_base_ref_counted_t* base) {                                \
    (void)base;                                                              \
    counter++;                                                               \
  }                                                                          \
  static int CEF_CALLBACK prefix##_release(                                  \
      struct _cef_base_ref_counted_t* base) {                                \
    (void)base;                                                              \
    if (--counter <= 0) {                                                    \
      counter = 0;                                                           \
      return 1;                                                              \
    }                                                                        \
    return 0;                                                                \
  }                                                                          \
  static int CEF_CALLBACK prefix##_has_one_ref(                              \
      struct _cef_base_ref_counted_t* base) {                                \
    (void)base;                                                              \
    return counter == 1;                                                     \
  }                                                                          \
  static int CEF_CALLBACK prefix##_has_at_least_one_ref(                     \
      struct _cef_base_ref_counted_t* base) {                                \
    (void)base;                                                              \
    return counter >= 1;                                                     \
  }

static int g_app_refs = 1;
static int g_client_refs = 1;
static int g_render_refs = 1;
static int g_life_refs = 1;
static int g_load_refs = 1;
static int g_display_refs = 1;

DEFINE_REFCOUNT(app, g_app_refs)
DEFINE_REFCOUNT(client, g_client_refs)
DEFINE_REFCOUNT(render, g_render_refs)
DEFINE_REFCOUNT(life, g_life_refs)
DEFINE_REFCOUNT(load, g_load_refs)
DEFINE_REFCOUNT(display, g_display_refs)

/* --------------------------------------------------------------- strings --- */

static void to_utf8(const cef_string_t* src, char* dst, size_t cap) {
  cef_string_utf8_t out = {0};
  dst[0] = '\0';
  if (!src || src->length == 0) {
    return;
  }
  if (cef_string_utf16_to_utf8(src->str, src->length, &out) && out.str) {
    snprintf(dst, cap, "%s", out.str);
    cef_string_utf8_clear(&out);
  }
}

/* ------------------------------------------------------- render handler --- */

static void CEF_CALLBACK render_get_view_rect(struct _cef_render_handler_t* self,
                                              struct _cef_browser_t* browser,
                                              cef_rect_t* rect) {
  (void)self;
  (void)browser;
  if (!rect) {
    return;
  }
  rect->x = 0;
  rect->y = 0;
  rect->width = g_width;
  rect->height = g_height;
}

static int CEF_CALLBACK render_get_screen_info(
    struct _cef_render_handler_t* self,
    struct _cef_browser_t* browser,
    cef_screen_info_t* info) {
  (void)self;
  (void)browser;
  if (!info) {
    return 0;
  }
  memset(info, 0, sizeof(*info));
  info->device_scale_factor = 1.0f;
  info->depth = 32;
  info->depth_per_component = 8;
  info->is_monochrome = 0;
  return 1;
}

static void CEF_CALLBACK render_on_paint(struct _cef_render_handler_t* self,
                                         struct _cef_browser_t* browser,
                                         cef_paint_element_type_t type,
                                         size_t dirty_count,
                                         const cef_rect_t* dirty_rects,
                                         const void* buffer,
                                         int width,
                                         int height) {
  (void)self;
  (void)browser;
  (void)dirty_count;
  (void)dirty_rects;
  if (type != PET_VIEW || !buffer || width <= 0 || height <= 0) {
    return;
  }

  size_t needed = (size_t)width * (size_t)height * 4u;
  if (needed > g_frame_cap) {
    unsigned char* grown = (unsigned char*)realloc(g_frame, needed);
    if (!grown) {
      set_error("frame buffer allocation failed");
      return;
    }
    g_frame = grown;
    g_frame_cap = needed;
  }
  memcpy(g_frame, buffer, needed);
  g_width = width;
  g_height = height;
  g_state |= CINDER_CEF_PAINTED;
}

static cef_render_handler_t g_render_handler = {
    .base = {.size = sizeof(cef_render_handler_t),
             .add_ref = render_add_ref,
             .release = render_release,
             .has_one_ref = render_has_one_ref,
             .has_at_least_one_ref = render_has_at_least_one_ref},
    .get_view_rect = render_get_view_rect,
    .get_screen_info = render_get_screen_info,
    .on_paint = render_on_paint,
};

/* ---------------------------------------------------- life span handler --- */

static void CEF_CALLBACK life_on_after_created(
    struct _cef_life_span_handler_t* self,
    struct _cef_browser_t* browser) {
  (void)self;
  (void)browser;
  g_state |= CINDER_CEF_CREATED;
}

static int CEF_CALLBACK life_do_close(struct _cef_life_span_handler_t* self,
                                      struct _cef_browser_t* browser) {
  (void)self;
  (void)browser;
  return 0; /* allow the close */
}

static void CEF_CALLBACK life_on_before_close(
    struct _cef_life_span_handler_t* self,
    struct _cef_browser_t* browser) {
  (void)self;
  (void)browser;
  g_state |= CINDER_CEF_CLOSED;
  g_browser = NULL;
}

static int CEF_CALLBACK life_on_before_popup(
    struct _cef_life_span_handler_t* self,
    struct _cef_browser_t* browser,
    struct _cef_frame_t* frame,
    int popup_id,
    const cef_string_t* target_url,
    const cef_string_t* target_frame_name,
    cef_window_open_disposition_t target_disposition,
    int user_gesture,
    const cef_popup_features_t* popupFeatures,
    struct _cef_window_info_t* windowInfo,
    struct _cef_client_t** client,
    struct _cef_browser_settings_t* settings,
    struct _cef_dictionary_value_t** extra_info,
    int* no_javascript_access) {
  (void)self;
  (void)browser;
  (void)frame;
  (void)popup_id;
  (void)target_url;
  (void)target_frame_name;
  (void)target_disposition;
  (void)user_gesture;
  (void)popupFeatures;
  (void)windowInfo;
  (void)client;
  (void)settings;
  (void)extra_info;
  (void)no_javascript_access;
  return 1; /* block popups in the spike */
}

static cef_life_span_handler_t g_life_handler = {
    .base = {.size = sizeof(cef_life_span_handler_t),
             .add_ref = life_add_ref,
             .release = life_release,
             .has_one_ref = life_has_one_ref,
             .has_at_least_one_ref = life_has_at_least_one_ref},
    .on_before_popup = life_on_before_popup,
    .on_after_created = life_on_after_created,
    .do_close = life_do_close,
    .on_before_close = life_on_before_close,
};

/* -------------------------------------------------------- load handler --- */

static void CEF_CALLBACK load_on_loading_state_change(
    struct _cef_load_handler_t* self,
    struct _cef_browser_t* browser,
    int isLoading,
    int canGoBack,
    int canGoForward) {
  (void)self;
  (void)browser;
  (void)canGoBack;
  (void)canGoForward;
  if (!isLoading) {
    g_state |= CINDER_CEF_LOADED;
  }
}

static cef_load_handler_t g_load_handler = {
    .base = {.size = sizeof(cef_load_handler_t),
             .add_ref = load_add_ref,
             .release = load_release,
             .has_one_ref = load_has_one_ref,
             .has_at_least_one_ref = load_has_at_least_one_ref},
    .on_loading_state_change = load_on_loading_state_change,
};

/* ----------------------------------------------------- display handler --- */

static void CEF_CALLBACK display_on_address_change(
    struct _cef_display_handler_t* self,
    struct _cef_browser_t* browser,
    struct _cef_frame_t* frame,
    const cef_string_t* url) {
  (void)self;
  (void)browser;
  (void)frame;
  to_utf8(url, g_url, sizeof(g_url));
}

static void CEF_CALLBACK display_on_title_change(
    struct _cef_display_handler_t* self,
    struct _cef_browser_t* browser,
    const cef_string_t* title) {
  (void)self;
  (void)browser;
  to_utf8(title, g_title, sizeof(g_title));
}

static cef_display_handler_t g_display_handler = {
    .base = {.size = sizeof(cef_display_handler_t),
             .add_ref = display_add_ref,
             .release = display_release,
             .has_one_ref = display_has_one_ref,
             .has_at_least_one_ref = display_has_at_least_one_ref},
    .on_address_change = display_on_address_change,
    .on_title_change = display_on_title_change,
};

/* -------------------------------------------------------------- client --- */

static struct _cef_render_handler_t* CEF_CALLBACK client_get_render_handler(
    struct _cef_client_t* self) {
  (void)self;
  return &g_render_handler;
}

static struct _cef_life_span_handler_t* CEF_CALLBACK client_get_life_span_handler(
    struct _cef_client_t* self) {
  (void)self;
  return &g_life_handler;
}

static struct _cef_load_handler_t* CEF_CALLBACK client_get_load_handler(
    struct _cef_client_t* self) {
  (void)self;
  return &g_load_handler;
}

static struct _cef_display_handler_t* CEF_CALLBACK client_get_display_handler(
    struct _cef_client_t* self) {
  (void)self;
  return &g_display_handler;
}

static cef_client_t g_client = {
    .base = {.size = sizeof(cef_client_t),
             .add_ref = client_add_ref,
             .release = client_release,
             .has_one_ref = client_has_one_ref,
             .has_at_least_one_ref = client_has_at_least_one_ref},
    .get_display_handler = client_get_display_handler,
    .get_life_span_handler = client_get_life_span_handler,
    .get_load_handler = client_get_load_handler,
    .get_render_handler = client_get_render_handler,
};

/* ---------------------------------------------------------------- app ---- */

static cef_app_t g_app = {
    .base = {.size = sizeof(cef_app_t),
             .add_ref = app_add_ref,
             .release = app_release,
             .has_one_ref = app_has_one_ref,
             .has_at_least_one_ref = app_has_at_least_one_ref},
};

/* --------------------------------------------------------------- public --- */

/* libcef derives its struct-version checks from the first cef_api_hash() call.
 * The C++ wrapper does this at startup; using the C API directly means we must
 * do it ourselves, and it must happen before cef_execute_process(), which
 * already converts the app struct. */
static int g_api_configured = 0;

static int configure_api_version(void) {
  if (g_api_configured) {
    return 1;
  }
  const char* api_hash = cef_api_hash(CEF_API_VERSION, 0);
  if (!api_hash) {
    set_error("cef_api_hash returned null");
    return 0;
  }
  if (strcmp(api_hash, CEF_API_HASH_PLATFORM) != 0) {
    set_error("CEF API hash mismatch: headers do not match libcef.so");
    return 0;
  }
  g_api_configured = 1;
  return 1;
}

int cinder_cef_execute_process(int argc, char** argv) {
  cef_main_args_t args;
  if (!configure_api_version()) {
    return -1;
  }
  memset(&args, 0, sizeof(args));
  args.argc = argc;
  args.argv = argv;
  return cef_execute_process(&args, &g_app, NULL);
}

int cinder_cef_init(int argc, char** argv, const char* resources_dir,
                    const char* locales_dir, const char* cache_dir) {
  cef_main_args_t args;
  cef_settings_t settings;

  memset(&args, 0, sizeof(args));
  args.argc = argc;
  args.argv = argv;

  memset(&settings, 0, sizeof(settings));
  settings.size = sizeof(cef_settings_t);
  settings.no_sandbox = 1;
  settings.windowless_rendering_enabled = 1;
  settings.log_severity = LOGSEVERITY_WARNING;
  settings.background_color = 0xFFFFFFFFu;
  if (resources_dir) {
    cef_string_from_utf8(resources_dir, strlen(resources_dir),
                         &settings.resources_dir_path);
  }
  if (locales_dir) {
    cef_string_from_utf8(locales_dir, strlen(locales_dir),
                         &settings.locales_dir_path);
  }
  if (cache_dir && cache_dir[0]) {
    cef_string_from_utf8(cache_dir, strlen(cache_dir), &settings.root_cache_path);
    cef_string_from_utf8(cache_dir, strlen(cache_dir), &settings.cache_path);
  }

  /* libcef derives its struct-version checks from the first cef_api_hash()
   * call. The C++ wrapper does this at startup; using the C API directly means
   * we must do it ourselves, or every CToCpp conversion reports version -1. */
  if (!configure_api_version()) {
    return 0;
  }

  if (!cef_initialize(&args, &settings, &g_app, NULL)) {
    set_error("cef_initialize failed");
    return 0;
  }
  return 1;
}

int cinder_cef_create_browser(const char* url, int width, int height) {
  cef_window_info_t window_info;
  cef_browser_settings_t browser_settings;
  cef_string_t url_str = {0};

  if (!url || width <= 0 || height <= 0) {
    set_error("invalid browser arguments");
    return 0;
  }

  g_width = width;
  g_height = height;

  memset(&window_info, 0, sizeof(window_info));
  window_info.size = sizeof(cef_window_info_t);
  window_info.windowless_rendering_enabled = 1;
  window_info.bounds.x = 0;
  window_info.bounds.y = 0;
  window_info.bounds.width = width;
  window_info.bounds.height = height;

  memset(&browser_settings, 0, sizeof(browser_settings));
  browser_settings.size = sizeof(cef_browser_settings_t);
  browser_settings.windowless_frame_rate = 30;

  cef_string_from_utf8(url, strlen(url), &url_str);

  int ok = cef_browser_host_create_browser(&window_info, &g_client, &url_str,
                                           &browser_settings, NULL, NULL);
  cef_string_clear(&url_str);
  if (!ok) {
    set_error("cef_browser_host_create_browser failed");
    return 0;
  }
  return 1;
}

void cinder_cef_pump(int ms) {
  int elapsed = 0;
  while (elapsed < ms) {
    cef_do_message_loop_work();
    usleep(1000);
    elapsed += 1;
  }
}

int cinder_cef_state(void) { return g_state; }

int cinder_cef_frame_size(int* width, int* height) {
  if (!g_frame || g_width <= 0 || g_height <= 0) {
    return 0;
  }
  if (width) {
    *width = g_width;
  }
  if (height) {
    *height = g_height;
  }
  return 1;
}

int cinder_cef_copy_frame(unsigned char* dst, int capacity) {
  if (!dst || !g_frame || capacity <= 0) {
    return 0;
  }
  size_t needed = (size_t)g_width * (size_t)g_height * 4u;
  if ((size_t)capacity < needed) {
    return 0;
  }
  memcpy(dst, g_frame, needed);
  return (int)needed;
}

const char* cinder_cef_title(void) { return g_title; }
const char* cinder_cef_url(void) { return g_url; }
const char* cinder_cef_last_error(void) { return g_error; }

const char* cinder_cef_version(void) { return CEF_VERSION; }

void cinder_cef_shutdown(void) {
  if (g_browser) {
    g_browser->get_host(g_browser)->close_browser(g_browser->get_host(g_browser), 1);
    g_browser = NULL;
  }
  cef_shutdown();
  free(g_frame);
  g_frame = NULL;
  g_frame_cap = 0;
  g_state = 0;
}
