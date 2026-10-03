# cros-codecs

BSD-3-Clause upstream https://github.com/chromeos/cros-codecs at
5ff6d693ffae0b36935b8fc13092c733b4c2646f. Its original license is retained.
One compatibility change: the initial placeholder decoder context is 64×64
instead of 16×16, which radeonsi rejects with VA_STATUS_ERROR_RESOLUTION_NOT_SUPPORTED.
Actual stream geometry replaces it as soon as the SPS is read.

Low-delay integration also advertises zero reorder frames and a one-frame DPB
in the low-delay encoder SPS. The H.264 decoder exposes `end_access_unit` to
submit the final picture of a complete transport access unit without flushing
reference state or waiting for another frame on a still desktop.
`force_keyframe` resets the low-delay predictor counter so it emits IDR with
SPS/PPS rather than a non-IDR I picture. At explicit access-unit boundaries,
zero-reorder streams present the completed DPB picture while retaining reference
entries for the next P picture.

The H.264 VA backend also submits a VA HRD buffer capped at 100 ms of the
CBR bitrate (half-full initially), rather than leaving VBV driver-default.
