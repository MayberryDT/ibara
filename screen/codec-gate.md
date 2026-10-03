# Codec and VPP Gate

The initial captured-dmabuf probe rejected AR24 in cros-codecs. Converting the
captured RGB dmabuf to NV12 with VAProfileNone/VAEntrypointVideoProc allowed
H.264 Main encoding on an Intel sender and decoding on Intel and AMD.

The pinned cros-codecs revision is `5ff6d693ffae0b36935b8fc13092c733b4c2646f`;
cros-libva is 0.0.12. The initial decoder context is 64×64 because radeonsi
rejects the upstream 16×16 placeholder. Decoder surfaces are allocated through
libva and exported as dmabufs. The RGB source and encoded dimensions are kept
separate so VPP can scale without cropping.

This gate establishes codec compatibility, not Stage 1 stream acceptance.
The sender and viewer need real-machine latency, loss, input lifecycle and
changing-screen soak proofs. No FFmpeg implementation or dependency belongs
to ibara-screen; the separately packaged Sunshine fallback retains its own
upstream dependencies.
