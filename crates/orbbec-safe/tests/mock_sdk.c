/* Deterministic C ABI fixture: no SDK, camera, ROS, or USB access. */
#include <stdint.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
typedef struct obj { int kind, refs, stream; } obj;
typedef void error;
static int live[6], active, calibration_live;
static unsigned char bytes[] = {16,128,32,128,64,90,80,240};
static obj *make(int kind) { obj *o=malloc(sizeof(*o)); o->kind=kind; o->refs=1; o->stream=0; live[kind]++; return o; }
static void release(obj *o) { if(!--o->refs){live[o->kind]--;free(o);} }
static void event(const char *name) {const char *path=getenv("GEMINIDEPTHPORT_MOCK_EVENTS");if(path){FILE *f=fopen(path,"a");if(f){fprintf(f,"%s\n",name);fclose(f);}}}
int ob_get_version(void){return 20903;}
obj *ob_create_context(error **e){(void)e;return make(0);}
void ob_delete_context(obj *o,error **e){(void)e;
#ifndef MOCK_MULTIPLE_CONTEXTS
if(calibration_live||live[1]||live[2]||live[3]||live[4]||live[5])abort();
#endif
release(o);event("context-delete");}
obj *ob_query_device_list(obj *c,error **e){(void)c;(void)e;return make(1);}
void ob_delete_device_list(obj *o,error **e){(void)e;if(live[2]||live[3]||live[4]||live[5])abort();release(o);event("list-delete");}
obj *ob_device_list_get_device_by_serial_number(obj *l,const char *serial,error **e){(void)l;if(strcmp(serial,"test-device")){*e=(error*)1;return NULL;}return make(2);}
void ob_delete_device(obj *o,error **e){(void)e;if(live[3]||live[4]||live[5])abort();release(o);event("device-delete");}
obj *ob_create_pipeline_with_device(obj *d,error **e){(void)d;(void)e;return make(3);}
void ob_delete_pipeline(obj *o,error **e){(void)e;if(active||live[5])abort();release(o);event("pipeline-delete");}
obj *ob_create_config(error **e){(void)e;return make(4);}
void ob_delete_config(obj *o,error **e){(void)e;release(o);event("config-delete");}
void ob_config_enable_video_stream(obj *c,int s,uint32_t w,uint32_t h,uint32_t fps,int format,error **e){(void)c;(void)s;if(w!=2||h!=2||fps!=30||(format!=0&&format!=8))*e=(error*)1;}
void ob_pipeline_start_with_config(obj *p,const obj *c,error **e){(void)p;(void)c;(void)e;active=1;event("start");}
void ob_pipeline_stop(obj *p,error **e){(void)p;(void)e;active=0;event("stop");}
obj *ob_pipeline_wait_for_frameset(obj *p,uint32_t ms,error **e){(void)p;(void)e;if(ms==1)return NULL;return make(5);}
obj *ob_frameset_get_color_frame(const obj *p,error **e){(void)p;(void)e;obj *f=make(5);f->stream=2;return f;}
obj *ob_frameset_get_depth_frame(const obj *p,error **e){(void)p;(void)e;obj *f=make(5);f->stream=3;return f;}
void ob_delete_frame(const obj *p,error **e){(void)e;release((obj*)p);event("frame-delete");}
void ob_frame_add_ref(const obj *p,error **e){(void)e;((obj*)p)->refs++;event("frame-add-ref");}
int ob_frame_get_format(const obj *p,error **e){(void)e;return p->stream==3?8:0;}
uint32_t ob_video_frame_get_width(const obj *p,error **e){(void)p;(void)e;return 2;}
uint32_t ob_video_frame_get_height(const obj *p,error **e){(void)p;(void)e;return 2;}
uint64_t ob_frame_get_index(const obj *p,error **e){(void)p;(void)e;return 42;}
uint64_t ob_frame_get_timestamp_us(const obj *p,error **e){(void)p;(void)e;return 1234;}
uint64_t ob_frame_get_system_timestamp_us(const obj *p,error **e){(void)p;(void)e;return 5678;}
uint8_t *ob_frame_get_data(const obj *p,error **e){(void)p;(void)e;return bytes;}
uint32_t ob_frame_get_data_size(const obj *p,error **e){(void)p;(void)e;return sizeof(bytes);}
float ob_depth_frame_get_value_scale(const obj *p,error **e){(void)p;(void)e;return 0.1f;}
const char *ob_error_get_message(const error *e){(void)e;return "mock SDK rejection";}
void ob_delete_error(error *e){(void)e;event("error-delete");}

#ifndef MOCK_NO_CALIBRATION
#pragma pack(push, 1)
typedef struct {float fx,fy,cx,cy; int16_t width,height;} intrinsic;
typedef struct {float k1,k2,k3,k4,k5,k6,p1,p2; int model;} distortion;
typedef struct {float rot[9],trans[3];} extrinsic;
#pragma pack(pop)
typedef struct {int stream;} profile;
static profile *make_profile(int stream){profile *p=malloc(sizeof(*p));p->stream=stream;calibration_live++;return p;}
obj *ob_pipeline_get_config(const obj *p,error **e){(void)p;(void)e;return make(4);}
profile *ob_config_get_enabled_stream_profile_list(const obj *c,error **e){(void)c;(void)e;return make_profile(0);}
void ob_delete_stream_profile_list(profile *p,error **e){(void)e;calibration_live--;free(p);event("profile-list-delete");}
uint32_t ob_stream_profile_list_get_count(const profile *p,error **e){(void)p;(void)e;return 2;}
profile *ob_stream_profile_list_get_profile(const profile *p,int index,error **e){(void)p;(void)e;return make_profile(index==0?2:3);}
profile *ob_frame_get_stream_profile(const obj *f,error **e){(void)e;return make_profile(f->stream);}
void ob_delete_stream_profile(profile *p,error **e){(void)e;calibration_live--;free(p);event("profile-delete");}
int ob_stream_profile_get_type(const profile *p,error **e){(void)e;return p->stream;}
int ob_stream_profile_get_format(const profile *p,error **e){(void)e;return p->stream==2?0:8;}
uint32_t ob_video_stream_profile_get_width(const profile *p,error **e){(void)p;(void)e;return 2;}
uint32_t ob_video_stream_profile_get_height(const profile *p,error **e){(void)p;(void)e;return 2;}
uint32_t ob_video_stream_profile_get_fps(const profile *p,error **e){(void)p;(void)e;return 30;}
intrinsic ob_video_stream_profile_get_intrinsic(const profile *p,error **e){(void)p;(void)e;return(intrinsic){1,2,.5f,.5f,2,2};}
distortion ob_video_stream_profile_get_distortion(const profile *p,error **e){(void)p;(void)e;return(distortion){.1f,0,0,0,0,0,0,0,3};}
extrinsic ob_stream_profile_get_extrinsic_to(const profile *source,profile *target,error **e){
  if(source->stream!=3||target->stream!=2){*e=(error*)1;return(extrinsic){{0},{0}};}
  return(extrinsic){{0,-1,0,1,0,0,0,0,1},{100,200,300}};
}
#endif

#ifndef MOCK_NO_CAPTURE_CONTROLS
#ifndef MOCK_CAPTURE_FAILURE
#define MOCK_CAPTURE_FAILURE 0
#endif
#ifndef MOCK_INITIAL_CLOCK
#define MOCK_INITIAL_CLOCK 0
#endif
static int global_enabled, host_clock=MOCK_INITIAL_CLOCK, clock_get_count;
static int capture_error(error **e,int stage){if(MOCK_CAPTURE_FAILURE==stage){*e=(error*)1;return 1;}return 0;}
int ob_context_get_timestamp_clock_type(const obj *c,error **e){(void)c;event("clock-get");clock_get_count++;if(capture_error(e,6)||(MOCK_CAPTURE_FAILURE==9&&clock_get_count>1)){*e=(error*)1;return 0;}return host_clock;}
void ob_context_set_timestamp_clock_type(obj *c,int clock,error **e){(void)c;if(active)abort();event(clock==0?"clock-realtime":"clock-monotonic");if(!capture_error(e,7)&&MOCK_CAPTURE_FAILURE!=8)host_clock=clock;}
void ob_pipeline_enable_frame_sync(obj *p,error **e){(void)p;if(active)abort();event("sync-on");(void)capture_error(e,4);}
void ob_pipeline_disable_frame_sync(obj *p,error **e){(void)p;if(active)abort();event("sync-off");(void)capture_error(e,4);}
void ob_config_set_frame_aggregate_output_mode(obj *c,int mode,error **e){(void)c;if(active)abort();event(mode==0?"aggregate-all":mode==2?"aggregate-any":mode==3?"aggregate-disabled":"aggregate-invalid");(void)capture_error(e,3);}
_Bool ob_device_is_global_timestamp_supported(const obj *d,error **e){(void)d;event("global-support");if(capture_error(e,1))return 0;
#ifdef MOCK_GLOBAL_UNSUPPORTED
return 0;
#else
return 1;
#endif
}
void ob_device_enable_global_timestamp(obj *d,_Bool enabled,error **e){(void)d;if(active)abort();event(enabled?"global-on":"global-off");if(!capture_error(e,2))global_enabled=enabled;}
#ifndef MOCK_NO_GLOBAL_READ
uint64_t ob_frame_get_global_timestamp_us(const obj *f,error **e){(void)f;if(capture_error(e,5))return 0;
#ifdef MOCK_GLOBAL_ZERO
return 0;
#else
return global_enabled?5000:0;
#endif
}
#endif
#endif
