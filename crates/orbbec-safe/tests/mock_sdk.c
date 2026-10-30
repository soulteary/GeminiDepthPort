#ifndef _POSIX_C_SOURCE
#define _POSIX_C_SOURCE 200809L
#endif
/* Deterministic C ABI fixture: no SDK, camera, ROS, or USB access. */
#include <stdint.h>
#include <stdatomic.h>
#include <pthread.h>
#include <time.h>
#include <math.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
typedef struct obj { int kind, refs, stream;uint64_t imu_sequence,imu_period_us; } obj;
typedef void error;
static _Atomic int live[6], active, calibration_live;
static unsigned char bytes[] = {16,128,32,128,64,90,80,240};
static obj *make(int kind) { obj *o=malloc(sizeof(*o)); o->kind=kind; o->refs=1; o->stream=0;o->imu_sequence=0;o->imu_period_us=5000; live[kind]++; return o; }
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
void ob_delete_frame(const obj *p,error **e){(void)e;int imu=p->stream>=4;release((obj*)p);if(!imu)event("frame-delete");if(imu&&getenv("GDP_MOCK_IMU_RELEASE_ERROR"))*e=(error*)1;}
void ob_frame_add_ref(const obj *p,error **e){(void)e;((obj*)p)->refs++;event("frame-add-ref");}
int ob_frame_get_format(const obj *p,error **e){(void)e;return p->stream==3?8:0;}
uint32_t ob_video_frame_get_width(const obj *p,error **e){(void)p;(void)e;return 2;}
uint32_t ob_video_frame_get_height(const obj *p,error **e){(void)p;(void)e;return 2;}
uint64_t ob_frame_get_index(const obj *p,error **e){(void)e;if(p->stream>=4){return p->imu_sequence;}return 42;}
uint64_t ob_frame_get_timestamp_us(const obj *p,error **e){(void)e;if(p->stream>=4){return p->imu_sequence*p->imu_period_us;}return 1234;}
uint64_t ob_frame_get_system_timestamp_us(const obj *p,error **e){(void)e;if(p->stream>=4){uint64_t sequence=p->imu_sequence;
if(getenv("GDP_MOCK_IMU_EQUAL_SYSTEM"))sequence=((sequence-1)/2)*2+1;
if(getenv("GDP_MOCK_IMU_SYSTEM_REGRESS")&&p->imu_sequence>2)sequence=0;
return 1700000000000000ULL+sequence*p->imu_period_us;}return 5678;}
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
typedef struct {int stream,imu_period_us;} profile;
static profile *make_profile(int stream){profile *p=malloc(sizeof(*p));p->stream=stream;p->imu_period_us=5000;calibration_live++;return p;}
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
if(f->stream>=4){uint64_t sequence=f->imu_sequence;
if(getenv("GDP_MOCK_IMU_GLOBAL_REGRESS")&&sequence>2)sequence=1;
return global_enabled?1700000000000000ULL+sequence*f->imu_period_us-2000:0;}
return global_enabled?5000:0;
#endif
}
#endif
#endif

#if !defined(MOCK_NO_IMU) && !defined(MOCK_NO_CALIBRATION)
typedef void (*imu_callback)(obj*,void*);
typedef struct {int stream,period_us;_Atomic int running;pthread_t thread;imu_callback callback;void *user;} imu_sensor;
#pragma pack(push,1)
typedef struct {float x,y,z;} imu_value;
#pragma pack(pop)
static _Atomic int imu_sensors;
imu_sensor *ob_device_get_sensor(obj *device,int type,error **e){(void)device;
 if(getenv("GDP_MOCK_IMU_UNSUPPORTED")){*e=(error*)1;return NULL;}
 imu_sensor *s=calloc(1,sizeof(*s));s->stream=type;atomic_fetch_add(&imu_sensors,1);event(type==4?"imu-accel-open":"imu-gyro-open");return s;}
void ob_delete_sensor(imu_sensor *s,error **e){(void)e;if(s->running)abort();free(s);atomic_fetch_sub(&imu_sensors,1);event("imu-sensor-delete");}
profile *ob_sensor_get_stream_profile_list(imu_sensor *s,error **e){(void)e;return make_profile(s->stream);}
profile *ob_stream_profile_list_get_accel_stream_profile(profile *list,int range,int rate,error **e){(void)range;
 int period_us=0;switch(rate){case 5:period_us=40000;break;case 6:period_us=20000;break;case 7:period_us=10000;break;case 8:period_us=5000;break;case 9:period_us=2000;break;case 10:period_us=1000;break;case 16:period_us=2500;break;case 17:period_us=1250;break;}
 if(getenv("GDP_MOCK_IMU_PROFILE_ERROR")||!period_us){*e=(error*)1;return NULL;}profile *p=make_profile(list->stream);p->imu_period_us=period_us;return p;}
profile *ob_stream_profile_list_get_gyro_stream_profile(profile *list,int range,int rate,error **e){return ob_stream_profile_list_get_accel_stream_profile(list,range,rate,e);}
static void *imu_loop(void *arg){imu_sensor *s=arg;uint64_t sequence=0;
 while(s->running){struct timespec delay={0,s->period_us*1000L};if(!getenv("GDP_MOCK_IMU_BURST"))nanosleep(&delay,NULL);
  if(getenv("GDP_MOCK_IMU_STALL")||(s->stream==5&&getenv("GDP_MOCK_IMU_GYRO_STALL")))continue;
  if(getenv("GDP_MOCK_IMU_NULL_FRAME")){s->callback(NULL,s->user);continue;}
  obj *f=make(5);f->stream=s->stream;f->imu_sequence=++sequence;f->imu_period_us=s->period_us;
  if(getenv("GDP_MOCK_IMU_REGRESS")&&sequence>2)f->imu_sequence=1;
  s->callback(f,s->user);if(sequence==129&&getenv("GDP_MOCK_IMU_BURST"))event("imu-burst-sent");
 }return NULL;}
void ob_sensor_start(imu_sensor *s,const profile *p,imu_callback cb,void *user,error **e){
 s->period_us=p->imu_period_us;
 if(s->stream==5&&getenv("GDP_MOCK_IMU_START_ERROR")){*e=(error*)1;return;}
 s->callback=cb;s->user=user;s->running=1;if(pthread_create(&s->thread,NULL,imu_loop,s)){s->running=0;*e=(error*)1;}event("imu-start");}
void ob_sensor_stop(imu_sensor *s,error **e){if(getenv("GDP_MOCK_IMU_STOP_ERROR")||(s->stream==5&&getenv("GDP_MOCK_IMU_GYRO_STOP_ERROR"))){*e=(error*)1;return;}if(atomic_exchange(&s->running,0))pthread_join(s->thread,NULL);event("imu-stop");}
int ob_frame_get_type(const obj *p,error **e){(void)e;if(getenv("GDP_MOCK_IMU_BAD_TYPE"))return -1;return p->stream==4?4:7;}
imu_value ob_accel_frame_get_value(const obj *p,error **e){(void)p;imu_value value={1.f,2.f,9.8f};if(getenv("GDP_MOCK_IMU_READ_ERROR"))*e=(error*)1;if(getenv("GDP_MOCK_IMU_NAN"))value.x=NAN;return value;}
imu_value ob_gyro_frame_get_value(const obj *p,error **e){return ob_accel_frame_get_value(p,e);}
#endif
