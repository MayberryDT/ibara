/* SPDX-License-Identifier: GPL-2.0-or-later
 * Private, version-pinned ibara integration. Not part of Mutter's stable API.
 */
#include "config.h"
#include <linux/input-event-codes.h>
#include <math.h>
#include <string.h>
#include "core/display-private.h"
#include "core/window-private.h"
#include "meta/meta-context.h"
#include "backends/meta-backend-private.h"
#include "backends/native/meta-backend-native.h"
#include "backends/native/meta-seat-native.h"
#include "backends/native/meta-virtual-input-device-native.h"
#include "compositor/meta-window-actor-private.h"
#include "wayland/meta-wayland-private.h"
#include "wayland/meta-wayland.h"
#include "wayland/meta-wayland-seat.h"
#include "wayland/meta-wayland-text-input.h"
#include "wayland/meta-wayland-input.h"
#include "wayland/meta-wayland-surface-private.h"
#include "core/ibara-input.h"

typedef struct {
  MetaDisplay *display;
  MetaWindow *target;
  ClutterVirtualInputDevice *keyboard, *pointer;
  char *generation;
  guint id, expiry;
  guint accepted, rejected;
  guint key_events, button_events, motion_events, text_events, scroll_events;
  gboolean active;
  gboolean keys[KEY_CNT], buttons[8];
  const char *reason;
  const ClutterEvent *cleanup_event;
} IbaraInput;

static gint next_id;

void
meta_ibara_input_init (MetaDisplay *display)
{
  if (!g_signal_lookup ("ibara-person-input", G_OBJECT_TYPE (display)))
    g_signal_new ("ibara-person-input", G_OBJECT_TYPE (display), G_SIGNAL_RUN_LAST,
                  0, NULL, NULL, NULL, G_TYPE_NONE, 0);
}

static guint
held_count (IbaraInput *input)
{
  guint count = 0;
  for (guint i = 0; i < G_N_ELEMENTS (input->keys); i++) count += input->keys[i];
  for (guint i = 0; i < G_N_ELEMENTS (input->buttons); i++) count += input->buttons[i];
  return count;
}

static void
stop_input (IbaraInput *input, const char *reason)
{
  if (input->active || !input->reason) input->reason = reason;
  input->active = FALSE;
  meta_virtual_input_device_native_ibara_set_active (0);
  g_clear_object (&input->keyboard);
  g_clear_object (&input->pointer);
}

static gboolean
expire_input (gpointer data)
{
  IbaraInput *input = data;
  input->expiry = 0;
  stop_input (input, "expired");
  return G_SOURCE_REMOVE;
}

static void
free_input (gpointer data)
{
  IbaraInput *input = data;
  if (input->expiry) g_source_remove (input->expiry);
  stop_input (input, "display_closed");
  g_clear_object (&input->target);
  g_free (input->generation);
  g_free (input);
}

static IbaraInput *
get_input (MetaDisplay *display)
{
  return g_object_get_data (G_OBJECT (display), "ibara-input");
}

static gboolean
target_ready (IbaraInput *input)
{
  MetaContext *context = meta_display_get_context (input->display);
  MetaBackend *backend = meta_context_get_backend (context);
  ClutterStage *stage = CLUTTER_STAGE (meta_backend_get_stage (backend));
  return input->target && !input->target->unmanaging &&
    !input->target->minimized && meta_window_showing_on_its_workspace (input->target) &&
    input->display->focus_window == input->target &&
    !clutter_stage_get_grab_actor (stage) &&
    !clutter_stage_get_key_focus (stage);
}

static gboolean
authorized (IbaraInput *input, const char *generation)
{
  if (!input || !input->active || g_strcmp0 (input->generation, generation)) return FALSE;
  if (!target_ready (input) ||
      meta_virtual_input_device_native_ibara_get_active () != input->id)
    {
      stop_input (input, "target_or_person_changed");
      return FALSE;
    }
  return TRUE;
}

gboolean
meta_display_ibara_input_idle (MetaDisplay *display)
{
  IbaraInput *input = get_input (display);
  return (!input || (!input->active && !held_count (input))) &&
    !meta_virtual_input_device_native_ibara_get_held () &&
    !meta_virtual_input_device_native_ibara_get_total_held ();
}

/**
 * meta_display_ibara_input_begin:
 * @display: the display
 * @window: the exact target window
 * @generation: a fresh private transaction token
 *
 * Returns: whether a guarded transaction was started; no input is sent.
 */
gboolean
meta_display_ibara_input_begin (MetaDisplay *display, MetaWindow *window,
                               const char *generation)
{
  MetaBackend *backend = meta_context_get_backend (meta_display_get_context (display));
  IbaraInput *input = get_input (display);
  ClutterSeat *seat;
  guint id;
  if (!META_IS_BACKEND_NATIVE (backend) || !generation || !*generation ||
      strlen (generation) > 64) return FALSE;
  for (const char *p = generation; *p; p++)
    if (!g_ascii_isalnum (*p) && *p != '-' && *p != '_') return FALSE;
  if (!meta_display_ibara_input_idle (display)) return FALSE;
  if (!input)
    {
      input = g_new0 (IbaraInput, 1);
      input->display = display;
      g_object_set_data_full (G_OBJECT (display), "ibara-input", input, free_input);
    }
  g_set_object (&input->target, window);
  if (!target_ready (input)) return FALSE;
  id = g_atomic_int_add (&next_id, 1) + 1;
  if (!id || id > G_MAXINT) return FALSE;
  if (input->expiry) g_source_remove (input->expiry);
  g_free (input->generation);
  input->generation = g_strdup (generation);
  input->id = id;
  input->accepted = input->rejected = 0;
  input->key_events = input->button_events = input->motion_events = input->text_events = 0;
  input->scroll_events = 0;
  input->reason = "active";
  seat = clutter_backend_get_default_seat (meta_backend_get_clutter_backend (backend));
  input->keyboard = meta_seat_native_create_ibara_device (seat, CLUTTER_KEYBOARD_DEVICE, id);
  input->pointer = meta_seat_native_create_ibara_device (seat, CLUTTER_POINTER_DEVICE, id);
  input->active = TRUE;
  meta_virtual_input_device_native_ibara_set_active (id);
  input->expiry = g_timeout_add_seconds (20, expire_input, input);
  return TRUE;
}

gboolean
meta_display_ibara_input_key (MetaDisplay *display, const char *generation,
                             guint key, gboolean pressed)
{
  IbaraInput *input = get_input (display);
  if (!authorized (input, generation) || !key || key >= KEY_CNT ||
      key >= BTN_MISC) return FALSE;
  clutter_virtual_input_device_notify_key (input->keyboard, g_get_monotonic_time (),
    key, pressed ? CLUTTER_KEY_STATE_PRESSED : CLUTTER_KEY_STATE_RELEASED);
  return TRUE;
}

gboolean
meta_display_ibara_input_text (MetaDisplay *display, const char *generation,
                              const char *text)
{
  IbaraInput *input = get_input (display);
  MetaWaylandCompositor *compositor;
  if (!authorized (input, generation) || !text || !*text || !g_utf8_validate (text, -1, NULL) ||
      strlen (text) > 4000) return FALSE;
  compositor = meta_context_get_wayland_compositor (meta_display_get_context (display));
  if (!meta_wayland_input_is_current_handler (compositor->seat->input_handler,
                                              compositor->seat->default_handler) ||
      !meta_wayland_text_input_ibara_commit (compositor->seat->text_input, input->target, text))
    return FALSE;
  input->accepted++;
  input->text_events++;
  return TRUE;
}

gboolean
meta_display_ibara_input_motion (MetaDisplay *display, const char *generation,
                                double x, double y)
{
  IbaraInput *input = get_input (display);
  MtkRectangle rect;
  if (!authorized (input, generation) || !isfinite (x) || !isfinite (y)) return FALSE;
  meta_window_get_frame_rect (input->target, &rect);
  if (x < rect.x || y < rect.y || x >= rect.x + rect.width || y >= rect.y + rect.height)
    return FALSE;
  clutter_virtual_input_device_notify_absolute_motion (input->pointer,
    g_get_monotonic_time (), x, y);
  return TRUE;
}

gboolean
meta_display_ibara_input_button (MetaDisplay *display, const char *generation,
                                guint button, gboolean pressed)
{
  IbaraInput *input = get_input (display);
  if (!authorized (input, generation) || button < 1 || button > 3) return FALSE;
  clutter_virtual_input_device_notify_button (input->pointer, g_get_monotonic_time (),
    button, pressed ? CLUTTER_BUTTON_STATE_PRESSED : CLUTTER_BUTTON_STATE_RELEASED);
  return TRUE;
}

gboolean
meta_display_ibara_input_scroll (MetaDisplay *display, const char *generation,
                                guint direction)
{
  IbaraInput *input = get_input (display);
  if (!authorized (input, generation) || direction > CLUTTER_SCROLL_RIGHT) return FALSE;
  clutter_virtual_input_device_notify_discrete_scroll (input->pointer,
    g_get_monotonic_time (), direction, CLUTTER_SCROLL_SOURCE_WHEEL);
  return TRUE;
}

void
meta_display_ibara_input_end (MetaDisplay *display, const char *generation)
{
  IbaraInput *input = get_input (display);
  if (input && !g_strcmp0 (input->generation, generation)) stop_input (input, "ended");
}

/**
 * meta_display_ibara_input_state:
 * @display: the display
 * Returns: (transfer full): JSON receipt; queued input is not delivery evidence.
 */
char *
meta_display_ibara_input_state (MetaDisplay *display)
{
  IbaraInput *input = get_input (display);
  if (!input) return g_strdup ("{\"version\":1,\"person_signal\":true,\"active\":false,\"settled\":true}");
  if (input->active && !authorized (input, input->generation)) {}
  return g_strdup_printf (
    "{\"version\":1,\"person_signal\":true,\"generation\":\"%s\",\"active\":%s,"
    "\"accepted\":%u,\"rejected\":%u,\"held\":%u,\"native_held\":%u,"
    "\"key_events\":%u,\"button_events\":%u,\"motion_events\":%u,\"text_events\":%u,\"scroll_events\":%u,"
    "\"settled\":%s,\"reason\":\"%s\"}", input->generation ? input->generation : "",
    input->active ? "true" : "false", input->accepted, input->rejected,
    held_count (input), meta_virtual_input_device_native_ibara_get_held (),
    input->key_events, input->button_events, input->motion_events, input->text_events, input->scroll_events,
    !input->active && !held_count (input) && !meta_virtual_input_device_native_ibara_get_held () ? "true" : "false",
    input->reason ? input->reason : "unavailable");
}

/* Guarded events bypass Shell capture, accessibility and global keybindings.
 * Only the original Wayland recipient may consume them; the final seat check
 * still runs inside handle_event after update has resolved its actual focus.
 */
static void
send_to_target (IbaraInput *input, const ClutterEvent *event)
{
  MetaWaylandCompositor *compositor = meta_context_get_wayland_compositor (
    meta_display_get_context (input->display));
  guint32 previous_time = input->display->current_time;
  input->display->current_time = clutter_event_get_time (event);
  meta_wayland_compositor_update (compositor, event);
  meta_wayland_compositor_handle_event (compositor, event);
  input->display->current_time = previous_time;
}

/* Runs in Mutter's first display filter before capture, text-input, keybindings
 * and Wayland seat update/delivery. Device tags survive queued stale events.
 * Cleanup updates seat state, and sends releases only to the original target.
 */
gboolean
meta_ibara_input_filter (MetaDisplay *display, const ClutterEvent *event,
                        ClutterActor *event_actor)
{
  IbaraInput *input = get_input (display);
  ClutterInputDevice *source = clutter_event_get_source_device (event);
  guint id = source ? GPOINTER_TO_UINT (g_object_get_data (G_OBJECT (source), "ibara-input-id")) : 0;
  ClutterEventType type = clutter_event_type (event);
  gboolean key = type == CLUTTER_KEY_PRESS || type == CLUTTER_KEY_RELEASE;
  gboolean button = type == CLUTTER_BUTTON_PRESS || type == CLUTTER_BUTTON_RELEASE;
  gboolean release = type == CLUTTER_KEY_RELEASE || type == CLUTTER_BUTTON_RELEASE;
  gboolean own_held = FALSE;
  guint code = key ? clutter_event_get_key_code (event) - 8 :
    button ? clutter_event_get_button (event) : 0;
  MetaWindowActor *actor;
  gboolean exact_pointer;
  if (input) input->cleanup_event = NULL;
  if (id && (type == CLUTTER_ENTER || type == CLUTTER_LEAVE)) return TRUE;
  if (!key && !button && type != CLUTTER_MOTION && type != CLUTTER_SCROLL &&
      type != CLUTTER_TOUCH_BEGIN && type != CLUTTER_TOUCH_UPDATE && type != CLUTTER_TOUCH_END)
    return FALSE;
  if (!id)
    {
      if (input && input->active && (key || button || type == CLUTTER_MOTION ||
          type == CLUTTER_SCROLL || type == CLUTTER_TOUCH_BEGIN))
        stop_input (input, "person_input");
      g_signal_emit_by_name (display, "ibara-person-input");
      return FALSE;
    }
  if (!input || input->id != id) return TRUE;
  if (key && code < KEY_CNT) own_held = input->keys[code];
  if (button && code < G_N_ELEMENTS (input->buttons)) own_held = input->buttons[code];
  actor = event_actor ? meta_window_actor_from_actor (event_actor) : NULL;
  exact_pointer = actor && meta_window_actor_get_meta_window (actor) == input->target;
  if (authorized (input, input->generation) && (key || exact_pointer))
    {
      if (key && code < KEY_CNT) input->keys[code] = !release;
      if (button && code < G_N_ELEMENTS (input->buttons)) input->buttons[code] = !release;
      input->accepted++;
      if (key) input->key_events++;
      if (button) input->button_events++;
      if (type == CLUTTER_MOTION) input->motion_events++;
      /* Stock generates smooth/discrete formats for different client versions.
       * Count one admitted notch, while letting Wayland select its format. */
      if (type == CLUTTER_SCROLL && clutter_event_get_scroll_direction (event) != CLUTTER_SCROLL_SMOOTH)
        input->scroll_events++;
      send_to_target (input, event);
      return TRUE;
    }
  if (input->active) stop_input (input, "recipient_changed");
  input->rejected++;
  if (release && own_held)
    {
      if (key) input->keys[code] = FALSE;
      if (button) input->buttons[code] = FALSE;
      if ((key && target_ready (input)) ||
          (button && exact_pointer && input->target && !input->target->unmanaging))
        {
          input->cleanup_event = event;
          send_to_target (input, event);
          return TRUE;
        }
      meta_wayland_compositor_update (
        meta_context_get_wayland_compositor (meta_display_get_context (display)), event);
    }
  return TRUE;
}

/* A second check at Wayland dispatch uses the actual focused surface and
 * refuses custom grabs. This closes synchronous focus changes in earlier
 * capture/keybinding callbacks, not just changes between queued events.
 */
gboolean
meta_ibara_input_delivery_allowed (MetaWaylandSeat *seat, const ClutterEvent *event)
{
  ClutterInputDevice *source = clutter_event_get_source_device (event);
  guint id = source ? GPOINTER_TO_UINT (g_object_get_data (G_OBJECT (source), "ibara-input-id")) : 0;
  MetaDisplay *display = meta_context_get_display (seat->compositor->context);
  IbaraInput *input = get_input (display);
  ClutterEventType type = clutter_event_type (event);
  gboolean key = type == CLUTTER_KEY_PRESS || type == CLUTTER_KEY_RELEASE;
  MetaWaylandSurface *surface;
  if (!id) return TRUE;
  if (!key && type != CLUTTER_BUTTON_PRESS && type != CLUTTER_BUTTON_RELEASE &&
      type != CLUTTER_MOTION && type != CLUTTER_SCROLL &&
      type != CLUTTER_TOUCH_BEGIN && type != CLUTTER_TOUCH_UPDATE &&
      type != CLUTTER_TOUCH_END) return TRUE;
  if (!input || id != input->id) return FALSE;
  surface = key ? meta_wayland_keyboard_get_focus_surface (seat->keyboard) :
    meta_wayland_pointer_get_focus_surface (seat->pointer);
  if (!surface || meta_wayland_surface_get_toplevel_window (surface) != input->target ||
      !meta_wayland_input_is_current_handler (seat->input_handler, seat->default_handler))
    {
      stop_input (input, !surface ? "wayland_recipient_missing" :
        meta_wayland_surface_get_toplevel_window (surface) != input->target ?
        "wayland_recipient_changed" : "wayland_grab_changed");
      return FALSE;
    }
  if (input->cleanup_event == event) return TRUE;
  return authorized (input, input->generation);
}

/* Wayland cursor requests can arrive after virtual-device disposal. Detach
 * the private sprite before Clutter frees it, leaving the stable person sprite. */
void
meta_ibara_input_sprite_removed (MetaBackend *backend, ClutterSprite *sprite)
{
  MetaWaylandCompositor *compositor = meta_context_get_wayland_compositor (
    meta_backend_get_context (backend));
  if (compositor)
    meta_wayland_pointer_ibara_detach_sprite (compositor->seat->pointer, sprite);
}
