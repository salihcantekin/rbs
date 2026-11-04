/* Field offsets for src/linux.rs (see offsets.sh). */
#include <stdio.h>
#include <stddef.h>
#include <libavcodec/avcodec.h>
#include <libavutil/frame.h>
#define P(s,f) printf("%s.%s %zu %zu\n", #s, #f, offsetof(s,f), sizeof(((s*)0)->f))
int main(void){
  printf("avcodec %d avutil %d\n", LIBAVCODEC_VERSION_MAJOR, LIBAVUTIL_VERSION_MAJOR);
  P(AVCodecContext, sample_fmt);
  P(AVCodecContext, frame_size);
  P(AVCodecContext, initial_padding);
  P(AVCodecContext, extradata);
  P(AVCodecContext, extradata_size);
  P(AVFrame, data); P(AVFrame, linesize); P(AVFrame, extended_data);
  P(AVFrame, nb_samples); P(AVFrame, format); P(AVFrame, pts); P(AVFrame, ch_layout);
  P(AVPacket, data); P(AVPacket, size); P(AVPacket, pts); P(AVPacket, dts); P(AVPacket, flags);
  printf("AVChannelLayout %zu\n", sizeof(AVChannelLayout));
  printf("FLTP %d\n", AV_SAMPLE_FMT_FLTP);
  return 0;
}
