/* Cinder L0 shim: flat C surface over the CEF C API. See cinder_cef_shim.h.
 *
 * Design notes:
 *  - Only the C API is used, so libcef_dll_wrapper (C++) is not needed at all.
 *  - Every handler is a static struct with a plain refcount; CEF add_refs them.
 *  - Views are a fixed table addressed by caller-supplied ids. Handlers map a
 *    cef_browser_t* back to its view by pointer, so no thread-local state and
 *    no per-view handler allocation is needed.
 *  - Events leave through one callback. The shim never calls into Rust except
 *    there, and never blocks waiting for an answer.
 */
#include "cinder_cef_shim.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>
#else
#include <pthread.h>
#include <unistd.h>
#endif

#include "include/capi/cef_app_capi.h"
#include "include/capi/cef_browser_capi.h"
#include "include/capi/cef_client_capi.h"
#include "include/capi/cef_display_handler_capi.h"
#include "include/capi/cef_life_span_handler_capi.h"
#include "include/capi/cef_load_handler_capi.h"
#include "include/capi/cef_render_handler_capi.h"
#include "include/capi/cef_request_handler_capi.h"
#include "include/cef_api_hash.h"
#include "include/cef_version.h"
#include "include/internal/cef_string.h"
#include "include/internal/cef_types.h"

/* ---------------------------------------------------------------- state --- */

#define CINDER_CEF_MAX_VIEWS 32
#define CINDER_CEF_MAX_BLOCKED_HOSTS 128

typedef struct {
  int id;
  int used;
  int state;
  int width;
  int height;
  int browser_id;
  unsigned char* frame;
  size_t frame_cap;
  char title[512];
  char url[2048];
  cef_browser_t* browser;
} cinder_view_t;

static cinder_view_t g_views[CINDER_CEF_MAX_VIEWS];
static int g_primary_view = -1;
/* Index of the view whose browser is being created. CEF reports the browser
 * pointer only in on_after_created, and creation is serialized on the engine
 * thread, so one slot is enough. */
static int g_pending_view = -1;
static char g_error[512] = {0};

static cinder_cef_event_fn g_event_cb = NULL;
static void* g_event_user = NULL;

static char g_blocked_hosts[CINDER_CEF_MAX_BLOCKED_HOSTS][256];
static int g_blocked_count = 0;
static int g_upgrade_https = 0;

/* Guards the policy copy and the browser→view lookup, both of which are read
 * from CEF's IO thread while the engine thread mutates them.
 *
 * MSVC has no <pthread.h>, so the lock is a thin wrapper over a slim reader/
 * writer lock on Windows and a pthread mutex elsewhere. */
#ifdef _WIN32
typedef SRWLOCK cinder_lock_t;
#define CINDER_LOCK_INIT SRWLOCK_INIT
static void lock_acquire(cinder_lock_t* l) { AcquireSRWLockExclusive(l); }
static void lock_release(cinder_lock_t* l) { ReleaseSRWLockExclusive(l); }
static void sleep_ms(unsigned ms) { Sleep(ms); }
#else
typedef pthread_mutex_t cinder_lock_t;
#define CINDER_LOCK_INIT PTHREAD_MUTEX_INITIALIZER
static void lock_acquire(cinder_lock_t* l) { pthread_mutex_lock(l); }
static void lock_release(cinder_lock_t* l) { pthread_mutex_unlock(l); }
static void sleep_ms(unsigned ms) { usleep(ms * 1000u); }
#endif

static cinder_lock_t g_lock = CINDER_LOCK_INIT;

static void set_error(const char* msg) {
  snprintf(g_error, sizeof(g_error), "%s", msg ? msg : "");
}

static void emit(int view_id, int kind, const char* a, const char* b) {
  if (g_event_cb) {
    g_event_cb(g_event_user, view_id, kind, a ? a : "", b ? b : "");
  }
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
static int g_request_refs = 1;
static int g_resource_refs = 1;

DEFINE_REFCOUNT(app, g_app_refs)
DEFINE_REFCOUNT(client, g_client_refs)
DEFINE_REFCOUNT(render, g_render_refs)
DEFINE_REFCOUNT(life, g_life_refs)
DEFINE_REFCOUNT(load, g_load_refs)
DEFINE_REFCOUNT(display, g_display_refs)
DEFINE_REFCOUNT(request, g_request_refs)
DEFINE_REFCOUNT(resource, g_resource_refs)

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

/* ----------------------------------------------------------- view table --- */

static cinder_view_t* view_by_id(int view_id) {
  int i;
  for (i = 0; i < CINDER_CEF_MAX_VIEWS; i++) {
    if (g_views[i].used && g_views[i].id == view_id) {
      return &g_views[i];
    }
  }
  return NULL;
}

static cinder_view_t* view_by_browser(struct _cef_browser_t* browser) {
  int i;
  int browser_id;
  if (!browser) {
    return NULL;
  }
  /* CEF does not promise the same cef_browser_t* instance in every callback,
   * so the pointer is tried first and the stable browser identifier second. */
  for (i = 0; i < CINDER_CEF_MAX_VIEWS; i++) {
    if (g_views[i].used && g_views[i].browser == browser) {
      return &g_views[i];
    }
  }
  browser_id = browser->get_identifier(browser);
  for (i = 0; i < CINDER_CEF_MAX_VIEWS; i++) {
    if (g_views[i].used && g_views[i].browser_id == browser_id) {
      return &g_views[i];
    }
  }
  return NULL;
}

/* Same lookup, safe to call from CEF's IO thread. Returns 0 when unknown. */
static int view_id_by_browser(struct _cef_browser_t* browser) {
  cinder_view_t* v;
  int id = 0;
  lock_acquire(&g_lock);
  v = view_by_browser(browser);
  if (v) {
    id = v->id;
  }
  lock_release(&g_lock);
  return id;
}

static cinder_view_t* view_alloc(int view_id) {
  int i;
  for (i = 0; i < CINDER_CEF_MAX_VIEWS; i++) {
    if (!g_views[i].used) {
      memset(&g_views[i], 0, sizeof(g_views[i]));
      g_views[i].used = 1;
      g_views[i].id = view_id;
      if (g_primary_view < 0) {
        g_primary_view = i;
      }
      return &g_views[i];
    }
  }
  return NULL;
}

static void view_release_frame(cinder_view_t* v) {
  if (!v) {
    return;
  }
  free(v->frame);
  v->frame = NULL;
  v->frame_cap = 0;
}

static cef_browser_host_t* acquire_host(cef_browser_t* browser) {
  if (!browser) {
    return NULL;
  }
  return browser->get_host(browser);
}

static void release_host(cef_browser_host_t* host) {
  if (host) {
    host->base.release(&host->base);
  }
}

static void release_frame(cef_frame_t* frame) {
  if (frame) {
    frame->base.release(&frame->base);
  }
}

/* ------------------------------------------------------- render handler --- */

static void CEF_CALLBACK render_get_view_rect(struct _cef_render_handler_t* self,
                                              struct _cef_browser_t* browser,
                                              cef_rect_t* rect) {
  cinder_view_t* v = view_by_browser(browser);
  (void)self;
  if (!rect) {
    return;
  }
  if (!v && g_pending_view >= 0) {
    v = &g_views[g_pending_view];
  }
  rect->x = 0;
  rect->y = 0;
  rect->width = v ? v->width : 0;
  rect->height = v ? v->height : 0;
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
  cinder_view_t* v = view_by_browser(browser);
  (void)self;
  (void)dirty_count;
  (void)dirty_rects;
  if (!v || type != PET_VIEW || !buffer || width <= 0 || height <= 0) {
    return;
  }

  size_t needed = (size_t)width * (size_t)height * 4u;
  if (needed > v->frame_cap) {
    unsigned char* grown = (unsigned char*)realloc(v->frame, needed);
    if (!grown) {
      set_error("frame buffer allocation failed");
      return;
    }
    v->frame = grown;
    v->frame_cap = needed;
  }
  memcpy(v->frame, buffer, needed);
  v->width = width;
  v->height = height;
  v->state |= CINDER_CEF_PAINTED;
  emit(v->id, CINDER_CEF_EV_PAINTED, NULL, NULL);
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
  cinder_view_t* v;
  (void)self;
  if (g_pending_view < 0 || g_pending_view >= CINDER_CEF_MAX_VIEWS) {
    return;
  }
  v = &g_views[g_pending_view];
  g_pending_view = -1;
  if (!v->used || !browser) {
    return;
  }
  v->browser = browser;
  v->browser_id = browser->get_identifier(browser);
  v->state |= CINDER_CEF_CREATED;
  emit(v->id, CINDER_CEF_EV_CREATED, v->url, NULL);
}

static int CEF_CALLBACK life_do_close(struct _cef_life_span_handler_t* self,
                                      struct _cef_browser_t* browser) {
  cinder_view_t* v = view_by_browser(browser);
  (void)self;
  if (v) {
    emit(v->id, CINDER_CEF_EV_CLOSE_REQUESTED, NULL, NULL);
  }
  /* Windowless rendering has no parent window to tear down, and CEF's docs are
   * explicit: returning 1 here leaves the browser partially closed and
   * on_before_close never runs. 0 destroys it immediately. */
  return 0;
}

static void CEF_CALLBACK life_on_before_close(
    struct _cef_life_span_handler_t* self,
    struct _cef_browser_t* browser) {
  cinder_view_t* v = view_by_browser(browser);
  (void)self;
  if (!v) {
    return;
  }
  v->state |= CINDER_CEF_CLOSED;
  v->browser = NULL;
  view_release_frame(v);
  emit(v->id, CINDER_CEF_EV_CLOSED, NULL, NULL);
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
  return 1; /* block popups until a tab model exists for them */
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
  cinder_view_t* v = view_by_browser(browser);
  (void)self;
  (void)canGoBack;
  (void)canGoForward;
  if (!v) {
    return;
  }
  if (isLoading) {
    v->state &= ~CINDER_CEF_LOADED;
    emit(v->id, CINDER_CEF_EV_LOAD_START, v->url, NULL);
  } else {
    v->state |= CINDER_CEF_LOADED;
    emit(v->id, CINDER_CEF_EV_LOAD_END, v->url, NULL);
  }
}

static void CEF_CALLBACK load_on_load_error(
    struct _cef_load_handler_t* self,
    struct _cef_browser_t* browser,
    struct _cef_frame_t* frame,
    cef_errorcode_t errorCode,
    const cef_string_t* errorText,
    const cef_string_t* failedUrl) {
  cinder_view_t* v = view_by_browser(browser);
  char code[32];
  char url[2048];
  (void)self;
  (void)frame;
  (void)errorText;
  if (!v) {
    return;
  }
  snprintf(code, sizeof(code), "%d", (int)errorCode);
  to_utf8(failedUrl, url, sizeof(url));
  emit(v->id, CINDER_CEF_EV_LOAD_ERROR, code, url);
}

static cef_load_handler_t g_load_handler = {
    .base = {.size = sizeof(cef_load_handler_t),
             .add_ref = load_add_ref,
             .release = load_release,
             .has_one_ref = load_has_one_ref,
             .has_at_least_one_ref = load_has_at_least_one_ref},
    .on_loading_state_change = load_on_loading_state_change,
    .on_load_error = load_on_load_error,
};

/* ----------------------------------------------------- display handler --- */

static void CEF_CALLBACK display_on_address_change(
    struct _cef_display_handler_t* self,
    struct _cef_browser_t* browser,
    struct _cef_frame_t* frame,
    const cef_string_t* url) {
  cinder_view_t* v = view_by_browser(browser);
  (void)self;
  (void)frame;
  if (!v) {
    return;
  }
  to_utf8(url, v->url, sizeof(v->url));
  emit(v->id, CINDER_CEF_EV_URL, v->url, NULL);
}

static void CEF_CALLBACK display_on_title_change(
    struct _cef_display_handler_t* self,
    struct _cef_browser_t* browser,
    const cef_string_t* title) {
  cinder_view_t* v = view_by_browser(browser);
  (void)self;
  if (!v) {
    return;
  }
  to_utf8(title, v->title, sizeof(v->title));
  emit(v->id, CINDER_CEF_EV_TITLE, v->title, NULL);
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

/* ------------------------------------------------------ request handler --- */

/* Lowercase host of an absolute URL, or "" when there is none. */
static void url_host(const char* url, char* out, size_t cap) {
  const char* p;
  size_t n = 0;
  out[0] = '\0';
  if (!url) {
    return;
  }
  p = strstr(url, "://");
  if (!p) {
    return;
  }
  p += 3;
  while (*p && *p != '/' && *p != ':' && *p != '?' && *p != '#' && n + 1 < cap) {
    out[n++] = (char)((*p >= 'A' && *p <= 'Z') ? *p + 32 : *p);
    p++;
  }
  out[n] = '\0';
}

static int host_is_blocked(const char* host) {
  int i;
  size_t host_len = strlen(host);
  if (host_len == 0) {
    return 0;
  }
  for (i = 0; i < g_blocked_count; i++) {
    size_t blocked_len = strlen(g_blocked_hosts[i]);
    if (host_len == blocked_len && strcmp(host, g_blocked_hosts[i]) == 0) {
      return 1;
    }
    if (host_len > blocked_len && host[host_len - blocked_len - 1] == '.' &&
        strcmp(host + host_len - blocked_len, g_blocked_hosts[i]) == 0) {
      return 1;
    }
  }
  return 0;
}

static cef_return_value_t CEF_CALLBACK resource_on_before_resource_load(
    struct _cef_resource_request_handler_t* self,
    struct _cef_browser_t* browser,
    struct _cef_frame_t* frame,
    struct _cef_request_t* request,
    struct _cef_callback_t* callback) {
  cef_string_userfree_t url_free;
  char url[2048];
  char host[256];
  int blocked = 0;
  int upgrade = 0;
  (void)self;
  (void)frame;
  (void)callback;

  if (!request) {
    return RV_CONTINUE;
  }
  url_free = request->get_url(request);
  url[0] = '\0';
  if (url_free) {
    to_utf8(url_free, url, sizeof(url));
    cef_string_userfree_utf16_free(url_free);
  }
  url_host(url, host, sizeof(host));
  /* Runs on the IO thread, so the policy copy is taken under the lock. */
  lock_acquire(&g_lock);
  blocked = host_is_blocked(host);
  upgrade = g_upgrade_https;
  lock_release(&g_lock);

  if (blocked) {
    emit(view_id_by_browser(browser), CINDER_CEF_EV_BLOCKED, url, NULL);
    return RV_CANCEL;
  }

  if (upgrade && strncmp(url, "http://", 7) == 0) {
    char upgraded[2048];
    cef_string_t upgraded_str = {0};
    snprintf(upgraded, sizeof(upgraded), "https://%s", url + 7);
    cef_string_from_utf8(upgraded, strlen(upgraded), &upgraded_str);
    request->set_url(request, &upgraded_str);
    cef_string_clear(&upgraded_str);
  }
  return RV_CONTINUE;
}

static cef_resource_request_handler_t g_resource_handler = {
    .base = {.size = sizeof(cef_resource_request_handler_t),
             .add_ref = resource_add_ref,
             .release = resource_release,
             .has_one_ref = resource_has_one_ref,
             .has_at_least_one_ref = resource_has_at_least_one_ref},
    .on_before_resource_load = resource_on_before_resource_load,
};

static struct _cef_resource_request_handler_t* CEF_CALLBACK
request_get_resource_request_handler(
    struct _cef_request_handler_t* self,
    struct _cef_browser_t* browser,
    struct _cef_frame_t* frame,
    struct _cef_request_t* request,
    int is_navigation,
    int is_download,
    const cef_string_t* request_initiator,
    int* disable_default_handling) {
  (void)self;
  (void)browser;
  (void)frame;
  (void)request;
  (void)is_navigation;
  (void)is_download;
  (void)request_initiator;
  (void)disable_default_handling;
  return &g_resource_handler;
}

static cef_request_handler_t g_request_handler = {
    .base = {.size = sizeof(cef_request_handler_t),
             .add_ref = request_add_ref,
             .release = request_release,
             .has_one_ref = request_has_one_ref,
             .has_at_least_one_ref = request_has_at_least_one_ref},
    .get_resource_request_handler = request_get_resource_request_handler,
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

static struct _cef_request_handler_t* CEF_CALLBACK client_get_request_handler(
    struct _cef_client_t* self) {
  (void)self;
  return &g_request_handler;
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
    .get_request_handler = client_get_request_handler,
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

/* cef_main_args_t carries argc/argv on POSIX but only the module instance on
 * Windows, so the fields have to be filled per platform. */
static void fill_main_args(cef_main_args_t* args, int argc, char** argv) {
  memset(args, 0, sizeof(*args));
#ifdef _WIN32
  (void)argc;
  (void)argv;
  args->instance = GetModuleHandleW(NULL);
#else
  args->argc = argc;
  args->argv = argv;
#endif
}

int cinder_cef_execute_process(int argc, char** argv) {
  cef_main_args_t args;
  if (!configure_api_version()) {
    return -1;
  }
  fill_main_args(&args, argc, argv);
  return cef_execute_process(&args, &g_app, NULL);
}

int cinder_cef_init(int argc, char** argv, const char* resources_dir,
                    const char* locales_dir, const char* cache_dir) {
  cef_main_args_t args;
  cef_settings_t settings;

  fill_main_args(&args, argc, argv);

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

void cinder_cef_set_event_callback(cinder_cef_event_fn cb, void* user_data) {
  g_event_cb = cb;
  g_event_user = user_data;
}

int cinder_cef_create_view(int view_id, const char* url, int width, int height) {
  cef_window_info_t window_info;
  cef_browser_settings_t browser_settings;
  cef_string_t url_str = {0};
  cinder_view_t* v;

  if (width <= 0 || height <= 0) {
    set_error("invalid view size");
    return 0;
  }
  if (view_by_id(view_id)) {
    set_error("view id already in use");
    return 0;
  }
  v = view_alloc(view_id);
  if (!v) {
    set_error("view table full");
    return 0;
  }
  v->width = width;
  v->height = height;
  if (url) {
    snprintf(v->url, sizeof(v->url), "%s", url);
  }

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

  cef_string_from_utf8(url ? url : "about:blank", strlen(url ? url : "about:blank"),
                       &url_str);

  g_pending_view = (int)(v - g_views);
  if (!cef_browser_host_create_browser(&window_info, &g_client, &url_str,
                                       &browser_settings, NULL, NULL)) {
    cef_string_clear(&url_str);
    g_pending_view = -1;
    v->used = 0;
    set_error("cef_browser_host_create_browser failed");
    return 0;
  }
  cef_string_clear(&url_str);
  return 1;
}

int cinder_cef_create_browser(const char* url, int width, int height) {
  return cinder_cef_create_view(1, url, width, height);
}

int cinder_cef_close_view(int view_id, int force) {
  cinder_view_t* v = view_by_id(view_id);
  cef_browser_host_t* host;
  if (!v || !v->browser) {
    return 0;
  }
  host = acquire_host(v->browser);
  if (!host) {
    return 0;
  }
  host->close_browser(host, force ? 1 : 0);
  release_host(host);
  return 1;
}

int cinder_cef_navigate(int view_id, const char* url) {
  cinder_view_t* v = view_by_id(view_id);
  cef_frame_t* frame;
  cef_string_t url_str = {0};
  if (!v || !v->browser || !url) {
    return 0;
  }
  frame = v->browser->get_main_frame(v->browser);
  if (!frame) {
    return 0;
  }
  cef_string_from_utf8(url, strlen(url), &url_str);
  frame->load_url(frame, &url_str);
  cef_string_clear(&url_str);
  release_frame(frame);
  return 1;
}

int cinder_cef_reload(int view_id) {
  cinder_view_t* v = view_by_id(view_id);
  if (!v || !v->browser) {
    return 0;
  }
  v->browser->reload(v->browser);
  return 1;
}

int cinder_cef_stop(int view_id) {
  cinder_view_t* v = view_by_id(view_id);
  if (!v || !v->browser) {
    return 0;
  }
  v->browser->stop_load(v->browser);
  return 1;
}

int cinder_cef_go_back(int view_id) {
  cinder_view_t* v = view_by_id(view_id);
  if (!v || !v->browser) {
    return 0;
  }
  v->browser->go_back(v->browser);
  return 1;
}

int cinder_cef_go_forward(int view_id) {
  cinder_view_t* v = view_by_id(view_id);
  if (!v || !v->browser) {
    return 0;
  }
  v->browser->go_forward(v->browser);
  return 1;
}

/* Whether the history has an entry to move to. The shell greys its buttons
 * with this instead of guessing from the URLs it has seen. */
int cinder_cef_can_go_back(int view_id) {
  cinder_view_t* v = view_by_id(view_id);
  if (!v || !v->browser) {
    return 0;
  }
  return v->browser->can_go_back(v->browser);
}

int cinder_cef_can_go_forward(int view_id) {
  cinder_view_t* v = view_by_id(view_id);
  if (!v || !v->browser) {
    return 0;
  }
  return v->browser->can_go_forward(v->browser);
}

int cinder_cef_set_zoom(int view_id, double level) {
  cinder_view_t* v = view_by_id(view_id);
  cef_browser_host_t* host;
  if (!v || !v->browser) {
    return 0;
  }
  host = acquire_host(v->browser);
  if (!host) {
    return 0;
  }
  host->set_zoom_level(host, level);
  release_host(host);
  return 1;
}

int cinder_cef_set_mute(int view_id, int muted) {
  cinder_view_t* v = view_by_id(view_id);
  cef_browser_host_t* host;
  if (!v || !v->browser) {
    return 0;
  }
  host = acquire_host(v->browser);
  if (!host) {
    return 0;
  }
  host->set_audio_muted(host, muted ? 1 : 0);
  release_host(host);
  return 1;
}

int cinder_cef_find(int view_id, const char* query, int forward) {
  cinder_view_t* v = view_by_id(view_id);
  cef_browser_host_t* host;
  cef_string_t query_str = {0};
  if (!v || !v->browser || !query) {
    return 0;
  }
  host = acquire_host(v->browser);
  if (!host) {
    return 0;
  }
  cef_string_from_utf8(query, strlen(query), &query_str);
  host->find(host, &query_str, forward ? 1 : 0, 0, 0);
  cef_string_clear(&query_str);
  release_host(host);
  return 1;
}

int cinder_cef_resize(int view_id, int width, int height) {
  cinder_view_t* v = view_by_id(view_id);
  cef_browser_host_t* host;
  if (!v || width <= 0 || height <= 0) {
    return 0;
  }
  v->width = width;
  v->height = height;
  if (!v->browser) {
    return 1;
  }
  host = acquire_host(v->browser);
  if (!host) {
    return 0;
  }
  host->was_resized(host);
  release_host(host);
  return 1;
}

int cinder_cef_set_visible(int view_id, int visible) {
  cinder_view_t* v = view_by_id(view_id);
  cef_browser_host_t* host;
  if (!v || !v->browser) {
    return 0;
  }
  host = acquire_host(v->browser);
  if (!host) {
    return 0;
  }
  host->was_hidden(host, visible ? 0 : 1);
  release_host(host);
  return 1;
}

int cinder_cef_set_focus(int view_id, int focused) {
  cinder_view_t* v = view_by_id(view_id);
  cef_browser_host_t* host;
  if (!v || !v->browser) {
    return 0;
  }
  host = acquire_host(v->browser);
  if (!host) {
    return 0;
  }
  host->set_focus(host, focused ? 1 : 0);
  release_host(host);
  return 1;
}

int cinder_cef_set_policy(const char* const* blocked_hosts, int count,
                          int upgrade_https) {
  int i;
  if (count < 0 || count > CINDER_CEF_MAX_BLOCKED_HOSTS) {
    set_error("blocked host count out of range");
    return 0;
  }
  lock_acquire(&g_lock);
  g_blocked_count = 0;
  for (i = 0; i < count; i++) {
    if (!blocked_hosts[i]) {
      continue;
    }
    snprintf(g_blocked_hosts[g_blocked_count], sizeof(g_blocked_hosts[0]), "%s",
             blocked_hosts[i]);
    g_blocked_count++;
  }
  g_upgrade_https = upgrade_https ? 1 : 0;
  lock_release(&g_lock);
  return 1;
}

void cinder_cef_pump(int ms) {
  int elapsed = 0;
  while (elapsed < ms) {
    cef_do_message_loop_work();
    sleep_ms(1);
    elapsed += 1;
  }
}

int cinder_cef_view_state(int view_id) {
  cinder_view_t* v = view_by_id(view_id);
  return v ? v->state : 0;
}

/* --------------------------------------------------------------- input --- */

/* Windowless rendering means CEF never sees an OS event, so the shell has to
 * forward every one of them. All of these are no-ops for an unknown view. */

void cinder_cef_send_mouse_move(int view_id, int x, int y, unsigned int modifiers,
                                int leave) {
  cinder_view_t* v = view_by_id(view_id);
  cef_browser_host_t* host;
  cef_mouse_event_t event;
  if (!v || !v->browser) {
    return;
  }
  host = acquire_host(v->browser);
  if (!host) {
    return;
  }
  memset(&event, 0, sizeof(event));
  event.x = x;
  event.y = y;
  event.modifiers = modifiers;
  host->send_mouse_move_event(host, &event, leave ? 1 : 0);
  release_host(host);
}

void cinder_cef_send_mouse_click(int view_id, int x, int y,
                                 unsigned int modifiers, int button, int up,
                                 int click_count) {
  cinder_view_t* v = view_by_id(view_id);
  cef_browser_host_t* host;
  cef_mouse_event_t event;
  if (!v || !v->browser) {
    return;
  }
  host = acquire_host(v->browser);
  if (!host) {
    return;
  }
  memset(&event, 0, sizeof(event));
  event.x = x;
  event.y = y;
  event.modifiers = modifiers;
  host->send_mouse_click_event(host, &event, (cef_mouse_button_type_t)button,
                               up ? 1 : 0, click_count);
  release_host(host);
}

void cinder_cef_send_mouse_wheel(int view_id, int x, int y,
                                 unsigned int modifiers, int delta_x,
                                 int delta_y) {
  cinder_view_t* v = view_by_id(view_id);
  cef_browser_host_t* host;
  cef_mouse_event_t event;
  if (!v || !v->browser) {
    return;
  }
  host = acquire_host(v->browser);
  if (!host) {
    return;
  }
  memset(&event, 0, sizeof(event));
  event.x = x;
  event.y = y;
  event.modifiers = modifiers;
  host->send_mouse_wheel_event(host, &event, delta_x, delta_y);
  release_host(host);
}

void cinder_cef_send_key(int view_id, int type, unsigned int modifiers,
                         int windows_key_code, int native_key_code,
                         int is_system_key, unsigned int character,
                         unsigned int unmodified_character) {
  cinder_view_t* v = view_by_id(view_id);
  cef_browser_host_t* host;
  cef_key_event_t event;
  if (!v || !v->browser) {
    return;
  }
  host = acquire_host(v->browser);
  if (!host) {
    return;
  }
  memset(&event, 0, sizeof(event));
  event.size = sizeof(event);
  event.type = (cef_key_event_type_t)type;
  event.modifiers = modifiers;
  event.windows_key_code = windows_key_code;
  event.native_key_code = native_key_code;
  event.is_system_key = is_system_key ? 1 : 0;
  event.character = (char16_t)character;
  event.unmodified_character = (char16_t)unmodified_character;
  host->send_key_event(host, &event);
  release_host(host);
}

int cinder_cef_view_frame_size(int view_id, int* width, int* height) {
  cinder_view_t* v = view_by_id(view_id);
  if (!v || !v->frame || v->width <= 0 || v->height <= 0) {
    return 0;
  }
  if (width) {
    *width = v->width;
  }
  if (height) {
    *height = v->height;
  }
  return 1;
}

int cinder_cef_copy_view_frame(int view_id, unsigned char* dst, int capacity) {
  cinder_view_t* v = view_by_id(view_id);
  size_t needed;
  if (!v || !dst || !v->frame || capacity <= 0) {
    return 0;
  }
  needed = (size_t)v->width * (size_t)v->height * 4u;
  if ((size_t)capacity < needed) {
    return 0;
  }
  memcpy(dst, v->frame, needed);
  return (int)needed;
}

const char* cinder_cef_view_title(int view_id) {
  cinder_view_t* v = view_by_id(view_id);
  return v ? v->title : "";
}

const char* cinder_cef_view_url(int view_id) {
  cinder_view_t* v = view_by_id(view_id);
  return v ? v->url : "";
}

int cinder_cef_state(void) {
  cinder_view_t* v =
      g_primary_view >= 0 ? &g_views[g_primary_view] : NULL;
  return (v && v->used) ? v->state : 0;
}

int cinder_cef_frame_size(int* width, int* height) {
  cinder_view_t* v =
      g_primary_view >= 0 ? &g_views[g_primary_view] : NULL;
  if (!v || !v->used) {
    return 0;
  }
  return cinder_cef_view_frame_size(v->id, width, height);
}

int cinder_cef_copy_frame(unsigned char* dst, int capacity) {
  cinder_view_t* v =
      g_primary_view >= 0 ? &g_views[g_primary_view] : NULL;
  if (!v || !v->used) {
    return 0;
  }
  return cinder_cef_copy_view_frame(v->id, dst, capacity);
}

const char* cinder_cef_title(void) {
  cinder_view_t* v =
      g_primary_view >= 0 ? &g_views[g_primary_view] : NULL;
  return (v && v->used) ? v->title : "";
}

const char* cinder_cef_url(void) {
  cinder_view_t* v =
      g_primary_view >= 0 ? &g_views[g_primary_view] : NULL;
  return (v && v->used) ? v->url : "";
}

const char* cinder_cef_last_error(void) { return g_error; }

const char* cinder_cef_version(void) { return CEF_VERSION; }

void cinder_cef_shutdown(void) {
  int i;
  for (i = 0; i < CINDER_CEF_MAX_VIEWS; i++) {
    if (g_views[i].used && g_views[i].browser) {
      cinder_cef_close_view(g_views[i].id, 1);
    }
  }
  cef_shutdown();
  for (i = 0; i < CINDER_CEF_MAX_VIEWS; i++) {
    view_release_frame(&g_views[i]);
    memset(&g_views[i], 0, sizeof(g_views[i]));
  }
  g_primary_view = -1;
  g_blocked_count = 0;
  g_upgrade_https = 0;
  g_event_cb = NULL;
  g_event_user = NULL;
}
