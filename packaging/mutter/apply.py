#!/usr/bin/env python3
"""Apply the private guard to the exact upstream Mutter 50.1 source.

Ubuntu's debian patches must be applied first. Every replacement is checked;
this deliberately refuses a new Mutter version instead of guessing its ABI.
"""
from pathlib import Path
import shutil
import sys
import re

root = Path(sys.argv[1]).resolve()
here = Path(__file__).resolve().parent
if "version: '50.1'" not in (root / 'meson.build').read_text():
    raise SystemExit('This integration requires Mutter 50.1')

# The production candidate must retain Ubuntu's exact patch series. Upstream
# source remains useful for inspection but is not an installable qualification.
debian = root / 'debian'
if debian.exists():
    changelog = debian / 'changelog'
    text = changelog.read_text()
    if not text.startswith('mutter (50.1-0ubuntu2.4) '):
        raise SystemExit('This Ubuntu integration requires source 50.1-0ubuntu2.4')
    changelog.write_text(text.replace('mutter (50.1-0ubuntu2.4) ',
                                     'mutter (50.1-0ubuntu2.4+ibara2) ', 1))
    symbols = debian / 'libmutter-18-0.symbols'
    text = symbols.read_text()
    anchor = ' meta_display_is_grabbed@Base 44.0'
    assert text.count(anchor) == 1
    entries = ''.join(f' meta_display_ibara_input_{name}@Base 50.1-0ubuntu2.4+ibara2\n'
                      for name in ['begin', 'button', 'end', 'idle', 'key', 'motion', 'scroll', 'state', 'text'])
    text = text.replace(anchor, entries + anchor)
    # Ubuntu backported this symbol while retaining its later upstream version.
    # The candidate must declare the actual package providing it.
    text = text.replace('clutter_backend_get_global_cursor_type@Base 50.1-0ubuntu3~',
                        'clutter_backend_get_global_cursor_type@Base 50.1-0ubuntu2.4+ibara2')
    symbols.write_text(text)

def change(path, before, after, count=1):
    file = root / path
    source = file.read_text()
    if source.count(before) != count:
        raise SystemExit(f'{path}: expected {count} source anchors, got {source.count(before)}')
    file.write_text(source.replace(before, after))

for name in ['ibara-input.c', 'ibara-input.h']:
    shutil.copyfile(here / name, root / 'src/core' / name)
change('src/meson.build', "  'core/events.c',", "  'core/events.c',\n  'core/ibara-input.c',")
change('src/core/events.c', '#include "core/events.h"', '#include "core/events.h"\n#include "core/ibara-input.h"')
change('src/core/events.c', '  display->current_time = clutter_event_get_time (event);',
       '  if (meta_ibara_input_filter (display, event, event_actor))\n    return CLUTTER_EVENT_STOP;\n\n  display->current_time = clutter_event_get_time (event);')
public = root / 'src/meta/display.h'
with public.open('a') as f:
    f.write('''
/* Private ibara guarded input API v1, Mutter 50.1 only. */
META_EXPORT gboolean meta_display_ibara_input_begin (MetaDisplay *display, MetaWindow *window, const char *generation);
META_EXPORT gboolean meta_display_ibara_input_idle (MetaDisplay *display);
META_EXPORT gboolean meta_display_ibara_input_key (MetaDisplay *display, const char *generation, guint key, gboolean pressed);
META_EXPORT gboolean meta_display_ibara_input_text (MetaDisplay *display, const char *generation, const char *text);
META_EXPORT gboolean meta_display_ibara_input_motion (MetaDisplay *display, const char *generation, double x, double y);
META_EXPORT gboolean meta_display_ibara_input_button (MetaDisplay *display, const char *generation, guint button, gboolean pressed);
META_EXPORT gboolean meta_display_ibara_input_scroll (MetaDisplay *display, const char *generation, guint direction);
META_EXPORT void meta_display_ibara_input_end (MetaDisplay *display, const char *generation);
META_EXPORT char *meta_display_ibara_input_state (MetaDisplay *display);
''')

native = 'src/backends/native/meta-virtual-input-device-native.c'
change(native, '  PROP_SLOT_BASE,', '  PROP_SLOT_BASE,\n  PROP_IBARA_ID,')
change(native, '  int button_count[KEY_CNT];', '  int button_count[KEY_CNT];\n  guint ibara_id;')
change(native, '  guint slot_base;', '  guint slot_base;\n  guint ibara_id;')
change(native, 'typedef struct\n{\n  uint64_t time_us;\n  double x;', '''static gint ibara_active;
static gint ibara_held;
/* Only the native input thread touches this list and its button counts. */
static GList *ibara_devices;
void meta_virtual_input_device_native_ibara_set_active (guint id) { g_atomic_int_set (&ibara_active, id); }
guint meta_virtual_input_device_native_ibara_get_active (void) { return g_atomic_int_get (&ibara_active); }
guint meta_virtual_input_device_native_ibara_get_held (void) { return g_atomic_int_get (&ibara_held); }

typedef struct
{
  uint64_t time_us;
  double x;''')
change(native, '  if (state)\n    return ++virtual_native->impl_state->button_count[button];',
       '  if (virtual_native->ibara_id) g_atomic_int_add (&ibara_held, state ? 1 : -1);\n  if (state)\n    return ++virtual_native->impl_state->button_count[button];')
change(native, '      switch (get_button_type (code))',
       '      if (impl_state->ibara_id) g_atomic_int_add (&ibara_held, -impl_state->button_count[code]);\n      impl_state->button_count[code] = 0;\n      switch (get_button_type (code))')
change(native, '  meta_seat_impl_remove_virtual_input_device (seat_impl, impl_state->device);',
       '  ibara_devices = g_list_remove (ibara_devices, impl_state);\n  meta_seat_impl_remove_virtual_input_device (seat_impl, impl_state->device);')
change(native, '  meta_seat_impl_add_virtual_input_device (seat_impl, impl_state->device);', '''  impl_state->ibara_id = virtual_native->ibara_id;
  if (impl_state->ibara_id)
    {
      if (meta_virtual_input_device_native_ibara_get_total_held ())
        meta_virtual_input_device_native_ibara_set_active (0);
      g_object_set_data (G_OBJECT (impl_state->device), "ibara-input-id", GUINT_TO_POINTER (impl_state->ibara_id));
      ibara_devices = g_list_prepend (ibara_devices, impl_state);
    }
  meta_seat_impl_add_virtual_input_device (seat_impl, impl_state->device);''')
change(native, '    case PROP_SLOT_BASE:\n      g_value_set_uint',
       '    case PROP_IBARA_ID:\n      g_value_set_uint (value, virtual_native->ibara_id);\n      break;\n    case PROP_SLOT_BASE:\n      g_value_set_uint')
change(native, '    case PROP_SLOT_BASE:\n      virtual_native->slot_base',
       '    case PROP_IBARA_ID:\n      virtual_native->ibara_id = g_value_get_uint (value);\n      break;\n    case PROP_SLOT_BASE:\n      virtual_native->slot_base')
change(native, '  g_object_class_install_properties (object_class, PROP_LAST, obj_props);', '''  obj_props[PROP_IBARA_ID] = g_param_spec_uint ("ibara-id", NULL, NULL, 0, G_MAXINT, 0,
    G_PARAM_READWRITE | G_PARAM_STATIC_STRINGS | G_PARAM_CONSTRUCT_ONLY);
  g_object_class_install_properties (object_class, PROP_LAST, obj_props);''')

# Refuse stale tasks before they alter native seat state. Every public guard
# operation uses one of these four callbacks. Cancellation cleanup is separate.
for name in ['notify_absolute_motion_in_impl', 'notify_button_in_impl', 'notify_key_in_impl', 'notify_discrete_scroll_in_impl']:
    file = root / native
    s = file.read_text()
    start = s.index(name + ' (GTask *task)')
    end = s.index('\nstatic ', start + 1)
    part = s[start:end]
    anchor = '  if (event->time_us == CLUTTER_CURRENT_TIME)'
    assert part.count(anchor) == 1
    check = '''  if (virtual_native->ibara_id &&
      virtual_native->ibara_id != meta_virtual_input_device_native_ibara_get_active ())
    {
      g_task_return_boolean (task, TRUE);
      return G_SOURCE_REMOVE;
    }

'''
    file.write_text(s[:start] + part.replace(anchor, check + anchor) + s[end:])

# Trial the XKB action before a tagged key can alter the shared native seat.
# Depressed modifiers are owned and settled; latched/locked state or a group
# switch is not an owned held key and must be refused before any effect.
with (root / native).open('a') as f:
    f.write("""
static gboolean
ibara_key_changes_shared_state (MetaSeatImpl *seat, guint evdev, guint pressed)
{
  struct xkb_state *current = seat->xkb;
  struct xkb_state *trial = xkb_state_new (xkb_state_get_keymap (current));
  const enum xkb_state_component components[] = { XKB_STATE_MODS_LATCHED, XKB_STATE_MODS_LOCKED,
    XKB_STATE_LAYOUT_DEPRESSED, XKB_STATE_LAYOUT_LATCHED, XKB_STATE_LAYOUT_LOCKED, XKB_STATE_LAYOUT_EFFECTIVE };
  gboolean changed = FALSE;
  if (!trial) return TRUE;
  xkb_state_update_mask (trial,
    xkb_state_serialize_mods (current, XKB_STATE_MODS_DEPRESSED),
    xkb_state_serialize_mods (current, XKB_STATE_MODS_LATCHED),
    xkb_state_serialize_mods (current, XKB_STATE_MODS_LOCKED),
    xkb_state_serialize_layout (current, XKB_STATE_LAYOUT_DEPRESSED),
    xkb_state_serialize_layout (current, XKB_STATE_LAYOUT_LATCHED),
    xkb_state_serialize_layout (current, XKB_STATE_LAYOUT_LOCKED));
  xkb_state_update_key (trial, evdev + 8, pressed ? XKB_KEY_DOWN : XKB_KEY_UP);
  for (guint i = 0; i < G_N_ELEMENTS (components); i++)
    changed |= i < 2 ?
      xkb_state_serialize_mods (trial, components[i]) != xkb_state_serialize_mods (current, components[i]) :
      xkb_state_serialize_layout (trial, components[i]) != xkb_state_serialize_layout (current, components[i]);
  xkb_state_unref (trial);
  return changed;
}
""")
# The definition is below the callback, so declare its exact private prototype.
change(native, 'static gint ibara_active;', 'static gboolean ibara_key_changes_shared_state (MetaSeatImpl *seat, guint evdev, guint pressed);\nstatic gint ibara_active;')
change(native, '  key_count = update_button_count_in_impl (virtual_native, event->key, event->key_state);', """  if (virtual_native->ibara_id && ibara_key_changes_shared_state (seat, event->key, event->key_state))
    {
      meta_virtual_input_device_native_ibara_person_input (seat, NULL);
      goto out;
    }
  key_count = update_button_count_in_impl (virtual_native, event->key, event->key_state);""")

# Settle agent modifiers/buttons before deriving the person's event state.
# Called only on the native input thread, before processing libinput input.
with (root / native).open('a') as f:
    f.write('''
void
meta_virtual_input_device_native_ibara_person_input (MetaSeatImpl *seat, ClutterInputDevice *source)
{
  if (source && g_object_get_data (G_OBJECT (source), "ibara-input-id")) return;
  if (!ibara_devices) return;
  meta_virtual_input_device_native_ibara_set_active (0);
  for (GList *l = ibara_devices; l; l = l->next)
    {
      ImplState *state = l->data;
      if (state->seat_impl != seat) continue;
      for (guint code = 0; code < KEY_CNT; code++)
        {
          if (!state->button_count[code]) continue;
          g_atomic_int_add (&ibara_held, -state->button_count[code]);
          state->button_count[code] = 0;
          if (get_button_type (code) == EVDEV_BUTTON_TYPE_KEY)
            meta_seat_impl_notify_key_in_impl (seat, state->device, g_get_monotonic_time (), code, CLUTTER_KEY_STATE_RELEASED, TRUE);
          else if (get_button_type (code) == EVDEV_BUTTON_TYPE_BUTTON)
            meta_seat_impl_notify_button_in_impl (seat, state->device, g_get_monotonic_time (), code, CLUTTER_BUTTON_STATE_RELEASED);
        }
    }
}
''')
with (root / 'src/backends/native/meta-virtual-input-device-native.h').open('a') as f:
    f.write('''
typedef struct _MetaSeatImpl MetaSeatImpl;
void meta_virtual_input_device_native_ibara_set_active (guint id);
guint meta_virtual_input_device_native_ibara_get_active (void);
guint meta_virtual_input_device_native_ibara_get_held (void);
guint meta_virtual_input_device_native_ibara_get_total_held (void);
void meta_virtual_input_device_native_ibara_person_input (MetaSeatImpl *seat, ClutterInputDevice *source);
''')
seat = 'src/backends/native/meta-seat-native.c'
change(seat, '  return g_object_new (META_TYPE_VIRTUAL_INPUT_DEVICE_NATIVE,',
       '  return g_object_new (META_TYPE_VIRTUAL_INPUT_DEVICE_NATIVE,')  # Assert factory anchor.
with (root / seat).open('a') as f:
    f.write('''
ClutterVirtualInputDevice *
meta_seat_native_create_ibara_device (ClutterSeat *seat, ClutterInputDeviceType type, guint id)
{
  MetaSeatNative *native = META_SEAT_NATIVE (seat);
  guint slot = bump_virtual_touch_slot_base (native);
  g_hash_table_add (native->reserved_virtual_slots, GUINT_TO_POINTER (slot));
  return g_object_new (META_TYPE_VIRTUAL_INPUT_DEVICE_NATIVE,
    "seat", seat, "slot-base", slot, "device-type", type, "ibara-id", id, NULL);
}
''')
with (root / 'src/backends/native/meta-seat-native.h').open('a') as f:
    f.write('\nClutterVirtualInputDevice *meta_seat_native_create_ibara_device (ClutterSeat *seat, ClutterInputDeviceType type, guint id);\n')
seat_impl = 'src/backends/native/meta-seat-impl.c'
change(seat_impl, '#include "config.h"', '#include "config.h"\n#include "clutter/clutter.h"\n#include "backends/native/meta-virtual-input-device-native.h"\nstatic gint ibara_touch_held;')
change(seat_impl, 'static int\nupdate_button_count (', 'static gint ibara_total_held;\nguint meta_virtual_input_device_native_ibara_get_total_held (void) { return g_atomic_int_get (&ibara_total_held) + g_atomic_int_get (&ibara_touch_held); }\n\nstatic int\nupdate_button_count (')
change(seat_impl, '  g_hash_table_insert (priv->touch_states, GINT_TO_POINTER (seat_slot),\n                       touch_state);',
       '  g_hash_table_insert (priv->touch_states, GINT_TO_POINTER (seat_slot),\n                       touch_state);\n  g_atomic_int_inc (&ibara_touch_held);')
change(seat_impl, '  g_hash_table_remove (priv->touch_states, GINT_TO_POINTER (seat_slot));',
       '  if (g_hash_table_remove (priv->touch_states, GINT_TO_POINTER (seat_slot)))\n    g_atomic_int_add (&ibara_touch_held, -1);')
change(seat_impl, '  g_clear_pointer (&priv->touch_states, g_hash_table_destroy);',
       '  if (priv->touch_states) g_atomic_int_add (&ibara_touch_held, -g_hash_table_size (priv->touch_states));\n  g_clear_pointer (&priv->touch_states, g_hash_table_destroy);')
change(seat_impl, '  if (state)\n    {\n      return ++seat_impl->button_count[button];',
       '  if (state && !seat_impl->button_count[button]) g_atomic_int_inc (&ibara_total_held);\n  if (!state && seat_impl->button_count[button] == 1) g_atomic_int_add (&ibara_total_held, -1);\n  if (state)\n    {\n      return ++seat_impl->button_count[button];')
change(seat_impl, '  if (process_base_event (seat_impl, event))', '''  if (libinput_event_get_type (event) != LIBINPUT_EVENT_DEVICE_ADDED &&
      libinput_event_get_type (event) != LIBINPUT_EVENT_DEVICE_REMOVED)
    meta_virtual_input_device_native_ibara_person_input (seat_impl, NULL);
  if (process_base_event (seat_impl, event))''')
# Owned raw keys must not toggle sticky/slow-key state or other person's
# accessibility preferences on the shared seat. Person keys keep the stock path.
change(seat_impl, '  should_ignore = is_a11y_modifier_first_click (seat_impl,',
       '  should_ignore = !g_object_get_data (G_OBJECT (device), "ibara-input-id") &&\n    is_a11y_modifier_first_click (seat_impl,')
change(seat_impl, '    meta_keyboard_a11y_process_event_in_impl (seat_impl->keyboard_a11y,',
       '    !g_object_get_data (G_OBJECT (device), "ibara-input-id") &&\n    meta_keyboard_a11y_process_event_in_impl (seat_impl->keyboard_a11y,')

for name in ['meta_seat_impl_notify_key_in_impl',
             'meta_seat_impl_notify_relative_motion_in_impl',
             'meta_seat_impl_notify_absolute_motion_in_impl',
             'meta_seat_impl_notify_button_in_impl',
             'meta_seat_impl_notify_scroll_continuous_in_impl',
             'meta_seat_impl_notify_discrete_scroll_in_impl',
             'meta_seat_impl_notify_touch_event_in_impl']:
    file = root / seat_impl
    s = file.read_text()
    start = s.index('\n' + name + ' (')
    brace = s.index('\n{', start)
    match = re.search(r'ClutterInputDevice\s+\*(\w+)', s[start:brace])
    assert match, name
    device = match.group(1)
    file.write_text(s[:brace + 2] + f'\n  meta_virtual_input_device_native_ibara_person_input (seat_impl, {device});' + s[brace + 2:])
change('src/wayland/meta-wayland-seat.c', '#include "config.h"', '#include "config.h"\n#include "core/ibara-input.h"')
change('src/wayland/meta-wayland-seat.c', '  return meta_wayland_input_handle_event (seat->input_handler, event);',
       '  if (!meta_ibara_input_delivery_allowed (seat, event)) return TRUE;\n  return meta_wayland_input_handle_event (seat->input_handler, event);')
with (root / 'src/wayland/meta-wayland-text-input.h').open('a') as f:
    f.write('\ngboolean meta_wayland_text_input_ibara_commit (MetaWaylandTextInput *text_input, MetaWindow *window, const char *text);\n')
with (root / 'src/wayland/meta-wayland-text-input.c').open('a') as f:
    f.write("""
/* Direct, bounded commit to the enabled exact surface, never the clipboard. */
gboolean
meta_wayland_text_input_ibara_commit (MetaWaylandTextInput *text_input, MetaWindow *window, const char *text)
{
  if (!text_input || !text_input->surface || !text_input->enabled ||
      !clutter_input_focus_is_focused (text_input->input_focus) ||
      wl_list_empty (&text_input->focus_resource_list) ||
      meta_wayland_surface_get_toplevel_window (text_input->surface) != window ||
      (text_input->preedit.string && *text_input->preedit.string) ||
      text_input->done_idle_id) return FALSE;
  meta_wayland_text_input_focus_commit_text (text_input->input_focus, text);
  meta_wayland_text_input_focus_flush_done (text_input->input_focus);
  return TRUE;
}
""")
# Tagged pointers have private native coordinates and a separate Clutter sprite.
# Neither source state nor backend visibility updates may move the person's cursor.
change('src/backends/native/meta-input-device-native.h',
       '  struct libinput_device *libinput_device;',
       '  graphene_point_t ibara_coords;\n  struct libinput_device *libinput_device;')
change(seat_impl, '  if (device)\n    {',
       '  if (device && g_object_get_data (G_OBJECT (device), "ibara-input-id"))\n    {\n      *coords = META_INPUT_DEVICE_NATIVE (device)->ibara_coords;\n      return;\n    }\n  if (device)\n    {')
change(seat_impl, '  g_rw_lock_writer_lock (&seat_impl->state_lock);\n\n  if (clutter_input_device_get_device_type (input_device) == CLUTTER_TABLET_DEVICE)',
       '  if (g_object_get_data (G_OBJECT (input_device), "ibara-input-id"))\n    {\n      META_INPUT_DEVICE_NATIVE (input_device)->ibara_coords = coords;\n      return;\n    }\n  g_rw_lock_writer_lock (&seat_impl->state_lock);\n\n  if (clutter_input_device_get_device_type (input_device) == CLUTTER_TABLET_DEVICE)')
change(seat_impl, '  constrain_coordinates (seat_impl, input_device, time_us, coords, &new_coords);',
       '  if (!g_object_get_data (G_OBJECT (input_device), "ibara-input-id"))\n    constrain_coordinates (seat_impl, input_device, time_us, coords, &new_coords);')
change(seat_impl, '  g_signal_emit (seat_impl, signals[POINTER_POSITION_CHANGED_IN_IMPL], 0,\n                 &priv->pointer_state);',
       '  if (!g_object_get_data (G_OBJECT (input_device), "ibara-input-id"))\n    g_signal_emit (seat_impl, signals[POINTER_POSITION_CHANGED_IN_IMPL], 0,\n                   &priv->pointer_state);')
change(seat_impl, '  if (clutter_input_device_get_device_type (input_device) == CLUTTER_TABLET_DEVICE)\n    button_state = &device_native->button_state;',
       '  if (g_object_get_data (G_OBJECT (input_device), "ibara-input-id") ||\n      clutter_input_device_get_device_type (input_device) == CLUTTER_TABLET_DEVICE)\n    button_state = &device_native->button_state;')
backend = 'src/backends/native/meta-clutter-backend-native.c'
change(backend, '  if (role == CLUTTER_SPRITE_ROLE_TABLET)\n    sprite_device = device;',
       '  if (role == CLUTTER_SPRITE_ROLE_TABLET ||\n      g_object_get_data (G_OBJECT (device), "ibara-input-id"))\n    sprite_device = device;')
change(backend, '  device_type = clutter_input_device_get_device_type (source_device);',
       '  device_type = clutter_input_device_get_device_type (source_device);\n  if (g_object_get_data (G_OBJECT (source_device), "ibara-input-id"))\n    return ensure_sprite (clutter_backend, stage, for_event,\n                          clutter_backend_native->stylus_sprites, source_device);')
change('clutter/clutter/clutter-stage.c',
       '          if (device_type != CLUTTER_TABLET_DEVICE &&',
       '          if (!g_object_get_data (G_OBJECT (source_device), "ibara-input-id") &&\n              device_type != CLUTTER_TABLET_DEVICE &&')
change('src/backends/meta-backend.c',
       '  update_last_device_from_event (backend, event);',
       '  ClutterInputDevice *source = clutter_event_get_source_device (event);\n  if (source && g_object_get_data (G_OBJECT (source), "ibara-input-id")) return;\n  update_last_device_from_event (backend, event);')
# Agent visibility is owned by ibara's named overlay, not a second theme cursor.
change(seat, '  if (clutter_sprite_get_role (sprite) == CLUTTER_SPRITE_ROLE_TOUCHPOINT)',
       '  ClutterInputDevice *source = clutter_sprite_get_sprite_device (sprite);\n  if (source && g_object_get_data (G_OBJECT (source), "ibara-input-id")) return NULL;\n  if (clutter_sprite_get_role (sprite) == CLUTTER_SPRITE_ROLE_TOUCHPOINT)')
change('src/wayland/meta-wayland-pointer.c', '#include \"config.h\"',
       '#include \"config.h\"\n#include \"clutter/clutter-mutter.h\"')
# Private agent focus is independent of the person's cursor visibility. Keep
# the stock focus requirement for every untagged sprite.
change('src/wayland/meta-wayland-pointer.c',
       '  g_return_if_fail (meta_cursor_tracker_get_pointer_visible (cursor_tracker) ||',
       '  ClutterInputDevice *ibara_source = pointer->sprite ?\n    clutter_sprite_get_sprite_device (pointer->sprite) : NULL;\n  g_return_if_fail ((ibara_source && g_object_get_data (G_OBJECT (ibara_source), "ibara-input-id")) ||\n                    meta_cursor_tracker_get_pointer_visible (cursor_tracker) ||')
change(backend, '#include "config.h"', '#include "config.h"\n#include "core/ibara-input.h"')
change(backend, '  meta_seat_native_remove_cursor_renderer (META_SEAT_NATIVE (seat), sprite);',
       '  meta_ibara_input_sprite_removed (clutter_backend_native->backend, sprite);\n  meta_seat_native_remove_cursor_renderer (META_SEAT_NATIVE (seat), sprite);')
with (root / 'src/wayland/meta-wayland-pointer.h').open('a') as f:
    f.write('\nvoid meta_wayland_pointer_ibara_detach_sprite (MetaWaylandPointer *pointer, ClutterSprite *sprite);\n')
with (root / 'src/wayland/meta-wayland-pointer.c').open('a') as f:
    f.write("""
/* Only the exact private sprite being destroyed may detach this recipient. */
void
meta_wayland_pointer_ibara_detach_sprite (MetaWaylandPointer *pointer, ClutterSprite *sprite)
{
  ClutterInputDevice *source = clutter_sprite_get_sprite_device (sprite);
  MetaBackend *backend;
  if (pointer->sprite != sprite || !source ||
      !g_object_get_data (G_OBJECT (source), "ibara-input-id")) return;
  meta_wayland_pointer_set_current (pointer, NULL);
  meta_wayland_pointer_set_implicit_grab_surface (pointer, NULL);
  meta_wayland_pointer_set_focus (pointer, NULL);
  backend = backend_from_pointer (pointer);
  pointer->sprite = clutter_backend_get_pointer_sprite (
    meta_backend_get_clutter_backend (backend),
    CLUTTER_STAGE (meta_backend_get_stage (backend)));
}
""")
change('src/core/events.c', '  display->clutter_event_filter = clutter_event_add_filter (NULL,',
       '  meta_ibara_input_init (display);\n  display->clutter_event_filter = clutter_event_add_filter (NULL,')
# Stock wheel events use the seat's person pointer coordinates. A tagged
# pointer must use its own position for both formats of each wheel notch.
file = root / 'src/backends/native/meta-seat-impl.c'
for name in ['notify_scroll', 'notify_discrete_scroll']:
    s = file.read_text()
    start = s.index(name + ' (ClutterInputDevice')
    end = s.index('\nstatic ', start + 1)
    part = s[start:end]
    anchor = 'priv->pointer_state,'
    assert part.count(anchor) == 1
    part = part.replace(anchor, '(g_object_get_data (G_OBJECT (input_device), "ibara-input-id") ?\n'
                       '                                      device_native->ibara_coords : priv->pointer_state),')
    anchor = 'clutter_input_device_get_device_type (input_device) == CLUTTER_TABLET_DEVICE'
    assert part.count(anchor) == 1
    part = part.replace(anchor, '(' + anchor + ' ||\n'
                        '      g_object_get_data (G_OBJECT (input_device), "ibara-input-id"))')
    file.write_text(s[:start] + part + s[end:])
print('Applied private ibara guard v1 to Mutter 50.1; not qualified.')
