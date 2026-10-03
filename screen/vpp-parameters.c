/* MIT. cros-libva does not expose va_vpp.h's pipeline parameter structure. */
#include <va/va.h>
#include <va/va_vpp.h>
#include <string.h>
VAStatus screen_vpp_parameters(VADisplay display, VAContextID context,
                              VASurfaceID rgb, unsigned mirror_vertical, VABufferID *buffer) {
    VAProcPipelineParameterBuffer parameters = {0};
    parameters.surface = rgb;
    parameters.mirror_state = mirror_vertical ? VA_MIRROR_VERTICAL : VA_MIRROR_NONE;
    parameters.surface_color_standard = VAProcColorStandardSRGB;
    parameters.output_color_standard = VAProcColorStandardBT709;
    return vaCreateBuffer(display, context, VAProcPipelineParameterBufferType,
                          sizeof(parameters), 1, &parameters, buffer);
}

/* Read only the scaled image for the software encoder. Scaling and RGB to
 * YUV conversion remain on the GPU; OpenH264 takes planar CPU pixels. */
VAStatus screen_vpp_read_yuv(VADisplay display, VASurfaceID surface,
                            unsigned width, unsigned height, unsigned char *pixels) {
    VAImageFormat format = {0};
    format.fourcc = VA_FOURCC_NV12;
    VAImage image;
    VAStatus status = vaCreateImage(display, &format, width, height, &image);
    if (status != VA_STATUS_SUCCESS) return status;
    status = vaGetImage(display, surface, 0, 0, width, height, image.image_id);
    void *mapped = NULL;
    if (status == VA_STATUS_SUCCESS) status = vaMapBuffer(display, image.buf, &mapped);
    if (status == VA_STATUS_SUCCESS) {
        const unsigned char *base = mapped;
        for (unsigned y = 0; y < height; ++y)
            memcpy(pixels + y * width, base + image.offsets[0] + y * image.pitches[0], width);
        unsigned char *u = pixels + width * height;
        unsigned char *v = u + width * height / 4;
        for (unsigned y = 0; y < height / 2; ++y) {
            const unsigned char *uv = base + image.offsets[1] + y * image.pitches[1];
            for (unsigned x = 0; x < width / 2; ++x) {
                u[y * width / 2 + x] = uv[x * 2];
                v[y * width / 2 + x] = uv[x * 2 + 1];
            }
        }
        status = vaUnmapBuffer(display, image.buf);
    }
    vaDestroyImage(display, image.image_id);
    return status;
}
