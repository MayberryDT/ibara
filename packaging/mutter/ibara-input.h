/* SPDX-License-Identifier: GPL-2.0-or-later */
#pragma once
#include "meta/display.h"
#include "wayland/meta-wayland-types.h"
gboolean meta_ibara_input_filter (MetaDisplay *display, const ClutterEvent *event,
                                 ClutterActor *event_actor);
gboolean meta_ibara_input_delivery_allowed (MetaWaylandSeat *seat, const ClutterEvent *event);

void meta_ibara_input_sprite_removed (MetaBackend *backend, ClutterSprite *sprite);

void meta_ibara_input_init (MetaDisplay *display);
