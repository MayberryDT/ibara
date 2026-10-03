# cros-libva 0.0.12

Vendored from the crates.io 0.0.12 source, licensed BSD-3-Clause. Local change:
VP9 encoding picture parameters use Default for additional/reserved fields,
so the initializer compiles against both older and newer libva headers.
H.264 behavior is unchanged. AMD packed headers are not enabled here.
