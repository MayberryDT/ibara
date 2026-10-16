from pathlib import Path
import sys
root=Path(sys.argv[1])
def change(p,a,b):
 s=p.read_text();assert s.count(a)==1,(p,a);p.write_text(s.replace(a,b))
p=root/'src/wayland/meta-wayland-input.h'
p.write_text(p.read_text()+'\ngpointer meta_wayland_input_current_handler_data (MetaWaylandInput *input, const MetaWaylandEventInterface *iface);\n')
p=root/'src/wayland/meta-wayland-input.c'
p.write_text(p.read_text()+'''
/* Private read-only inspection of an exact registered handler interface. */
gpointer
meta_wayland_input_current_handler_data (MetaWaylandInput *input,
                                         const MetaWaylandEventInterface *iface)
{
  MetaWaylandEventHandler *handler;
  if (wl_list_empty (&input->event_handler_list)) return NULL;
  handler = wl_container_of (input->event_handler_list.next, handler, link);
  return handler->iface == iface ? handler->user_data : NULL;
}
''')
p=root/'src/wayland/meta-wayland-popup.h'
p.write_text(p.read_text()+'\ngboolean meta_wayland_popup_grab_is_target (MetaWaylandSeat *seat, MetaWindow *target);\n')
p=root/'src/wayland/meta-wayland-popup.c'
p.write_text(p.read_text()+'''
/* Admit only a known current popup for the exact toplevel and client. */
gboolean
meta_wayland_popup_grab_is_target (MetaWaylandSeat *seat, MetaWindow *target)
{
  MetaWaylandPopupGrab *grab = meta_wayland_input_current_handler_data (
    seat->input_handler, &popup_event_interface);
  MetaWaylandSurface *surface;
  if (!grab || grab->seat != seat || !target || wl_list_empty (&grab->all_popups))
    return FALSE;
  surface = meta_wayland_popup_grab_get_top_popup (grab);
  return meta_wayland_surface_get_toplevel_window (surface) == target &&
    meta_wayland_surface_get_client (surface) == grab->grab_client;
}
''')
