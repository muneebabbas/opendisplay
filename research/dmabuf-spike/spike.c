// Zero-copy feasibility spike: ext-image-copy-capture (dmabuf) -> gbm -> VA-API DRM PRIME 2 import -> VPP NV12.
// Build: ./build.sh   Run: ./spike | tee out.txt
#define _GNU_SOURCE
#include <wayland-client.h>
#include "ext-image-capture-source-v1-client.h"
#include "ext-image-copy-capture-v1-client.h"
#include "linux-dmabuf-v1-client.h"
#include <gbm.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
#include <va/va_vpp.h>
#include <drm_fourcc.h>
#include <linux/dma-buf.h>
#include <sys/ioctl.h>
#include <sys/sysmacros.h>
#include <sys/stat.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static double now_ms(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1e3+t.tv_nsec/1e6;}
static double cpu_ms(void){struct timespec t;clock_gettime(CLOCK_THREAD_CPUTIME_ID,&t);return t.tv_sec*1e3+t.tv_nsec/1e6;}

// ---------------------------------------------------------------- summary
static char summary[16384]; static size_t sumlen;
#define SUM(...) do { sumlen += snprintf(summary+sumlen, sizeof summary - sumlen, __VA_ARGS__); } while(0)

// ---------------------------------------------------------------- names
static const char *fourcc_str(uint32_t f){ static char b[4][8]; static int i; char *s=b[i++&3];
  s[0]=f&0xff; s[1]=(f>>8)&0xff; s[2]=(f>>16)&0xff; s[3]=(f>>24)&0xff; s[4]=0; return s; }
static const char *mod_name(uint64_t m){
  switch(m){
  case DRM_FORMAT_MOD_LINEAR: return "LINEAR";
  case DRM_FORMAT_MOD_INVALID: return "INVALID(implicit)";
  case I915_FORMAT_MOD_X_TILED: return "I915_X_TILED";
  case I915_FORMAT_MOD_Y_TILED: return "I915_Y_TILED";
  case I915_FORMAT_MOD_Yf_TILED: return "I915_Yf_TILED";
  case I915_FORMAT_MOD_Y_TILED_CCS: return "I915_Y_TILED_CCS";
  case I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS: return "I915_Y_TILED_GEN12_RC_CCS";
  case I915_FORMAT_MOD_Y_TILED_GEN12_MC_CCS: return "I915_Y_TILED_GEN12_MC_CCS";
  case I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC: return "I915_Y_TILED_GEN12_RC_CCS_CC";
  case I915_FORMAT_MOD_4_TILED: return "I915_4_TILED";
  case I915_FORMAT_MOD_4_TILED_DG2_RC_CCS: return "I915_4_TILED_DG2_RC_CCS";
  case I915_FORMAT_MOD_4_TILED_DG2_MC_CCS: return "I915_4_TILED_DG2_MC_CCS";
  case I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC: return "I915_4_TILED_DG2_RC_CCS_CC";
  case I915_FORMAT_MOD_4_TILED_MTL_RC_CCS: return "I915_4_TILED_MTL_RC_CCS";
  case I915_FORMAT_MOD_4_TILED_MTL_MC_CCS: return "I915_4_TILED_MTL_MC_CCS";
  case I915_FORMAT_MOD_4_TILED_MTL_RC_CCS_CC: return "I915_4_TILED_MTL_RC_CCS_CC";
  case I915_FORMAT_MOD_4_TILED_LNL_CCS: return "I915_4_TILED_LNL_CCS (Xe2 flat-CCS compressed)";
  case I915_FORMAT_MOD_4_TILED_BMG_CCS: return "I915_4_TILED_BMG_CCS (Xe2 dGPU compressed)";
  default: return "?";
  }
}

// ---------------------------------------------------------------- wayland state
#define MAXOUT 8
struct out { struct wl_output *o; char name[64]; int w,h; };
static struct out outs[MAXOUT]; static int nouts;
static struct wl_display *dpy;
static struct ext_output_image_capture_source_manager_v1 *srcmgr;
static struct ext_image_copy_capture_manager_v1 *capmgr;
static struct zwp_linux_dmabuf_v1 *dmabuf; static uint32_t dmabuf_ver;
static uint64_t dmabuf_glob_mods[64]; static int ndmabuf_glob_mods;

static void o_geom(void*d,struct wl_output*o,int32_t x,int32_t y,int32_t pw,int32_t ph,int32_t sp,const char*mk,const char*md,int32_t t){}
static void o_mode(void*d,struct wl_output*o,uint32_t f,int32_t w,int32_t h,int32_t r){ struct out*u=d; if(f&WL_OUTPUT_MODE_CURRENT){u->w=w;u->h=h;} }
static void o_done(void*d,struct wl_output*o){}
static void o_scale(void*d,struct wl_output*o,int32_t s){}
static void o_name(void*d,struct wl_output*o,const char*n){ struct out*u=d; snprintf(u->name,sizeof u->name,"%s",n); }
static void o_desc(void*d,struct wl_output*o,const char*n){}
static const struct wl_output_listener out_l={o_geom,o_mode,o_done,o_scale,o_name,o_desc};

static void dm_format(void*d,struct zwp_linux_dmabuf_v1*z,uint32_t f){}
static void dm_mod(void*d,struct zwp_linux_dmabuf_v1*z,uint32_t f,uint32_t hi,uint32_t lo){
  if(f==DRM_FORMAT_XRGB8888 && ndmabuf_glob_mods<64) dmabuf_glob_mods[ndmabuf_glob_mods++]=((uint64_t)hi<<32)|lo; }
static const struct zwp_linux_dmabuf_v1_listener dm_l={dm_format,dm_mod};

static void reg_global(void*d,struct wl_registry*r,uint32_t name,const char*iface,uint32_t ver){
  if(!strcmp(iface,wl_output_interface.name) && nouts<MAXOUT){
    struct out*u=&outs[nouts++]; u->o=wl_registry_bind(r,name,&wl_output_interface,ver<4?ver:4); wl_output_add_listener(u->o,&out_l,u);
  } else if(!strcmp(iface,ext_output_image_capture_source_manager_v1_interface.name)){
    srcmgr=wl_registry_bind(r,name,&ext_output_image_capture_source_manager_v1_interface,1);
  } else if(!strcmp(iface,ext_image_copy_capture_manager_v1_interface.name)){
    capmgr=wl_registry_bind(r,name,&ext_image_copy_capture_manager_v1_interface,1);
  } else if(!strcmp(iface,zwp_linux_dmabuf_v1_interface.name)){
    dmabuf_ver = ver<3?ver:3; // v3: still gets format/modifier events (v4 drops them in favour of feedback)
    dmabuf=wl_registry_bind(r,name,&zwp_linux_dmabuf_v1_interface,dmabuf_ver); zwp_linux_dmabuf_v1_add_listener(dmabuf,&dm_l,NULL);
    printf("registry: zwp_linux_dmabuf_v1 advertised v%u, bound v%u\n",ver,dmabuf_ver);
  }
}
static void reg_remove(void*d,struct wl_registry*r,uint32_t n){}
static const struct wl_registry_listener reg_l={reg_global,reg_remove};

// Dispatch until *flag becomes nonzero or timeout. Returns 0 on success, -1 timeout.
static int dispatch_until(volatile int *flag, int timeout_ms){
  double end=now_ms()+timeout_ms;
  while(!*flag){
    while(wl_display_prepare_read(dpy)!=0) wl_display_dispatch_pending(dpy);
    if(*flag){ wl_display_cancel_read(dpy); break; }
    wl_display_flush(dpy);
    int left=(int)(end-now_ms()); if(left<=0){ wl_display_cancel_read(dpy); return -1; }
    struct pollfd p={wl_display_get_fd(dpy),POLLIN,0};
    int r=poll(&p,1,left);
    if(r>0) wl_display_read_events(dpy); else wl_display_cancel_read(dpy);
    if(wl_display_dispatch_pending(dpy)<0){ fprintf(stderr,"wayland error %d\n",wl_display_get_error(dpy)); exit(1);}
  }
  return 0;
}

// ---------------------------------------------------------------- session
#define MAXFMT 16
struct sess {
  struct ext_image_capture_source_v1 *src; struct ext_image_copy_capture_session_v1 *s;
  int w,h, done, stopped; int seq;
  uint32_t shm[32]; int nshm;
  dev_t dev; int have_dev;
  uint32_t fmt[MAXFMT]; uint64_t mods[MAXFMT][64]; int nmods[MAXFMT]; int nfmt;
  char order[512];
};
static void s_size(void*d,struct ext_image_copy_capture_session_v1*s,uint32_t w,uint32_t h){ struct sess*x=d; x->w=w;x->h=h;
  printf("  [%d] buffer_size %ux%u\n",x->seq++,w,h); strcat(x->order,"buffer_size "); }
static void s_shm(void*d,struct ext_image_copy_capture_session_v1*s,uint32_t f){ struct sess*x=d; if(x->nshm<32)x->shm[x->nshm++]=f;
  // wl_shm formats: 0=ARGB8888 1=XRGB8888, else fourcc
  printf("  [%d] shm_format %u (%s)\n",x->seq++,f, f==0?"ARGB8888":f==1?"XRGB8888":fourcc_str(f)); strcat(x->order,"shm_format "); }
static void s_dev(void*d,struct ext_image_copy_capture_session_v1*s,struct wl_array*a){ struct sess*x=d;
  if(a->size>=sizeof(dev_t)){ memcpy(&x->dev,a->data,sizeof(dev_t)); x->have_dev=1; }
  printf("  [%d] dmabuf_device (array %zu bytes) dev_t=%u:%u\n",x->seq++,a->size,major(x->dev),minor(x->dev)); strcat(x->order,"dmabuf_device "); }
static void s_fmt(void*d,struct ext_image_copy_capture_session_v1*s,uint32_t f,struct wl_array*a){ struct sess*x=d;
  printf("  [%d] dmabuf_format %s (0x%08x), %zu modifiers:\n",x->seq++,fourcc_str(f),f,a->size/8); strcat(x->order,"dmabuf_format ");
  int i=x->nfmt<MAXFMT?x->nfmt++:MAXFMT-1; x->fmt[i]=f; x->nmods[i]=0;
  uint64_t *m; wl_array_for_each(m,a){ printf("        0x%016llx %s\n",(unsigned long long)*m,mod_name(*m)); if(x->nmods[i]<64) x->mods[i][x->nmods[i]++]=*m; }
}
static void s_done(void*d,struct ext_image_copy_capture_session_v1*s){ struct sess*x=d; x->done=1; printf("  [%d] done\n",x->seq++); strcat(x->order,"done"); }
static void s_stopped(void*d,struct ext_image_copy_capture_session_v1*s){ struct sess*x=d; x->stopped=1; printf("  stopped!\n"); }
static const struct ext_image_copy_capture_session_v1_listener sess_l={s_size,s_shm,s_dev,s_fmt,s_done,s_stopped};

static void open_session(struct sess *x, struct wl_output *o, uint32_t opts){
  memset(x,0,sizeof *x);
  x->src=ext_output_image_capture_source_manager_v1_create_source(srcmgr,o);
  x->s=ext_image_copy_capture_manager_v1_create_session(capmgr,x->src,opts);
  ext_image_copy_capture_session_v1_add_listener(x->s,&sess_l,x);
  if(dispatch_until(&x->done,3000)) printf("  TIMEOUT waiting for session done\n");
}

static char render_node[128];
static void map_dev(dev_t dv){
  char p[256]; snprintf(p,sizeof p,"/sys/dev/char/%u:%u/device/drm",major(dv),minor(dv));
  DIR *d=opendir(p); printf("dmabuf_device %u:%u -> %s:",major(dv),minor(dv),p);
  if(!d){ printf(" (cannot open)\n"); return; }
  struct dirent*e; while((e=readdir(d))){ if(e->d_name[0]=='.')continue; printf(" %s",e->d_name);
    if(!strncmp(e->d_name,"renderD",7)) snprintf(render_node,sizeof render_node,"/dev/dri/%s",e->d_name); }
  closedir(d);
  // which node is the dev_t itself?
  const char *nodes[]={"/dev/dri/card0","/dev/dri/renderD128"};
  for(int i=0;i<2;i++){ struct stat st; if(!stat(nodes[i],&st)&&st.st_rdev==dv) printf("  [dev_t is %s]",nodes[i]); }
  printf("\n  -> render node: %s\n",render_node);
}

// ---------------------------------------------------------------- gbm buffers
static struct gbm_device *gbm;
struct buf {
  struct gbm_bo *bo; uint32_t fmt; uint64_t mod; int nplanes; uint32_t stride[4], offset[4]; int w,h;
  struct wl_buffer *wb; int wb_state; // 1 created, -1 failed
  VASurfaceID vasurf; int va_ok; uint32_t va_fourcc;
  char label[96];
};

static int alloc_buf(struct buf *b, int w, int h, uint32_t fmt, const uint64_t *mods, int nmods, const char *label){
  memset(b,0,sizeof *b); b->vasurf=VA_INVALID_SURFACE; b->w=w; b->h=h; b->fmt=fmt; snprintf(b->label,sizeof b->label,"%s",label);
  // filter INVALID out of the list (gbm rejects it)
  uint64_t m2[64]; int n2=0; for(int i=0;i<nmods;i++) if(mods[i]!=DRM_FORMAT_MOD_INVALID) m2[n2++]=mods[i];
  b->bo=gbm_bo_create_with_modifiers2(gbm,w,h,fmt,m2,n2,GBM_BO_USE_RENDERING);
  if(!b->bo){ printf("gbm alloc %-28s %s FAILED errno=%d (%s)\n",label,fourcc_str(fmt),errno,strerror(errno)); return -1; }
  b->mod=gbm_bo_get_modifier(b->bo); b->nplanes=gbm_bo_get_plane_count(b->bo);
  printf("gbm alloc %-28s %s %dx%d -> modifier 0x%016llx %s, planes=%d",label,fourcc_str(fmt),w,h,(unsigned long long)b->mod,mod_name(b->mod),b->nplanes);
  for(int i=0;i<b->nplanes;i++){ b->stride[i]=gbm_bo_get_stride_for_plane(b->bo,i); b->offset[i]=gbm_bo_get_offset(b->bo,i);
    printf(" [p%d stride=%u offset=%u]",i,b->stride[i],b->offset[i]); }
  int fd=gbm_bo_get_fd(b->bo); off_t sz=lseek(fd,0,SEEK_END); close(fd);
  printf(" dmabuf size=%lld\n",(long long)sz);
  return 0;
}

static void p_created(void*d,struct zwp_linux_buffer_params_v1*p,struct wl_buffer*wb){ struct buf*b=d; b->wb=wb; b->wb_state=1; }
static void p_failed(void*d,struct zwp_linux_buffer_params_v1*p){ struct buf*b=d; b->wb_state=-1; }
static const struct zwp_linux_buffer_params_v1_listener params_l={p_created,p_failed};

static int make_wl_buffer(struct buf *b){
  struct zwp_linux_buffer_params_v1 *p=zwp_linux_dmabuf_v1_create_params(dmabuf);
  zwp_linux_buffer_params_v1_add_listener(p,&params_l,b);
  int fds[4];
  for(int i=0;i<b->nplanes;i++){ fds[i]=gbm_bo_get_fd_for_plane(b->bo,i);
    zwp_linux_buffer_params_v1_add(p,fds[i],i,b->offset[i],b->stride[i],b->mod>>32,b->mod&0xffffffff); }
  // Use create (async, failure is an event) rather than create_immed (failure is a fatal protocol error).
  zwp_linux_buffer_params_v1_create(p,b->w,b->h,b->fmt,0);
  int r=dispatch_until(&b->wb_state,2000);
  for(int i=0;i<b->nplanes;i++) close(fds[i]);
  zwp_linux_buffer_params_v1_destroy(p);
  printf("  wl_buffer for %s: %s\n",b->label, r?"TIMEOUT": b->wb_state>0?"created":"FAILED (params.failed)");
  return b->wb_state>0?0:-1;
}

// ---------------------------------------------------------------- capture
struct frame { int ready, failed, reason, done; double t_ready; int ndamage; uint32_t transform; int sync_state; double t_pres; };
static struct buf *cur_buf;
static void f_transform(void*d,struct ext_image_copy_capture_frame_v1*f,uint32_t t){ ((struct frame*)d)->transform=t; }
static void f_damage(void*d,struct ext_image_copy_capture_frame_v1*f,int32_t x,int32_t y,int32_t w,int32_t h){ ((struct frame*)d)->ndamage++; }
static void f_pt(void*d,struct ext_image_copy_capture_frame_v1*f,uint32_t hi,uint32_t lo,uint32_t ns){
  ((struct frame*)d)->t_pres=(((uint64_t)hi<<32)|lo)*1e3+ns/1e6; } // assumes CLOCK_MONOTONIC presentation clock
// sync_state: 0 = no fence / signalled, 1 = unsignalled fence present, -errno on ioctl failure
static int probe_implicit_fence(struct gbm_bo *bo, uint32_t flags){
  int fd=gbm_bo_get_fd(bo);
  struct dma_buf_export_sync_file a={.flags=flags,.fd=-1};
  int r=ioctl(fd,DMA_BUF_IOCTL_EXPORT_SYNC_FILE,&a); int res;
  if(r){ res=-errno; } else { struct pollfd p={a.fd,POLLIN,0}; int pr=poll(&p,1,0); res = pr==0 ? 1 : 0; close(a.fd); }
  close(fd); return res;
}
static void f_ready(void*d,struct ext_image_copy_capture_frame_v1*f){ struct frame*x=d; x->t_ready=now_ms();
  // probe immediately in the event handler, before anything else touches the buffer
  x->sync_state=probe_implicit_fence(cur_buf->bo,DMA_BUF_SYNC_READ);
  x->ready=1; x->done=1; }
static void f_failed(void*d,struct ext_image_copy_capture_frame_v1*f,uint32_t r){ struct frame*x=d; x->failed=1; x->reason=r; x->done=1; }
static const struct ext_image_copy_capture_frame_v1_listener frame_l={f_transform,f_damage,f_pt,f_ready,f_failed};

static const char *fail_reason(int r){ return r==0?"unknown":r==1?"buffer_constraints":r==2?"stopped":"?"; }
static const char *sync_str(int s){ static char b[64]; if(s==1)return "UNSIGNALLED fence present"; if(s==0)return "no pending fence (signalled/none)";
  snprintf(b,sizeof b,"export_sync_file failed: %s",strerror(-s)); return b; }

// returns ms capture->ready, or -1
static double capture(struct sess *x, struct buf *b, struct frame *fr){
  memset(fr,0,sizeof *fr); cur_buf=b;
  struct ext_image_copy_capture_frame_v1 *f=ext_image_copy_capture_session_v1_create_frame(x->s);
  ext_image_copy_capture_frame_v1_add_listener(f,&frame_l,fr);
  ext_image_copy_capture_frame_v1_attach_buffer(f,b->wb);
  ext_image_copy_capture_frame_v1_damage_buffer(f,0,0,b->w,b->h);
  double t0=now_ms();
  ext_image_copy_capture_frame_v1_capture(f);
  int r=dispatch_until(&fr->done,3000);
  ext_image_copy_capture_frame_v1_destroy(f);
  if(r){ printf("  capture %s: TIMEOUT (no ready/failed in 3 s)\n",b->label); return -1; }
  if(fr->failed){ printf("  capture %s: FAILED reason=%d (%s)\n",b->label,fr->reason,fail_reason(fr->reason)); return -1; }
  double dt=fr->t_ready-t0;
  printf("  capture %s: READY in %.2f ms (presentation_time->ready %.2f ms; damage rects=%d, transform=%u); implicit fence right after ready: %s\n",b->label,dt,fr->t_pres?fr->t_ready-fr->t_pres:-1,fr->ndamage,fr->transform,sync_str(fr->sync_state));
  return dt;
}

// ---------------------------------------------------------------- VA
static VADisplay va; static VAConfigID vacfg; static VAContextID vactx; static VASurfaceID nv12;
static int VW,VH;

static VAStatus va_import(struct buf *b, uint32_t va_fourcc, VASurfaceID *out){
  VADRMPRIMESurfaceDescriptor desc; memset(&desc,0,sizeof desc);
  int fd=gbm_bo_get_fd(b->bo); off_t sz=lseek(fd,0,SEEK_END);
  desc.fourcc=va_fourcc; desc.width=b->w; desc.height=b->h;
  desc.num_objects=1; desc.objects[0].fd=fd; desc.objects[0].size=sz; desc.objects[0].drm_format_modifier=b->mod;
  desc.num_layers=1; desc.layers[0].drm_format=b->fmt; desc.layers[0].num_planes=b->nplanes;
  for(int i=0;i<b->nplanes;i++){ desc.layers[0].object_index[i]=0; desc.layers[0].offset[i]=b->offset[i]; desc.layers[0].pitch[i]=b->stride[i]; }
  VASurfaceAttrib at[2]; memset(at,0,sizeof at);
  at[0].type=VASurfaceAttribMemoryType; at[0].flags=VA_SURFACE_ATTRIB_SETTABLE; at[0].value.type=VAGenericValueTypeInteger; at[0].value.value.i=VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
  at[1].type=VASurfaceAttribExternalBufferDescriptor; at[1].flags=VA_SURFACE_ATTRIB_SETTABLE; at[1].value.type=VAGenericValueTypePointer; at[1].value.value.p=&desc;
  VAStatus s=vaCreateSurfaces(va,VA_RT_FORMAT_RGB32,b->w,b->h,out,1,at,2);
  close(fd); // driver has its own GEM handle now
  return s;
}

static VAStatus vpp(VASurfaceID in, int w, int h, int sync){
  VAProcPipelineParameterBuffer pp; memset(&pp,0,sizeof pp);
  VARectangle r={0,0,w,h};
  pp.surface=in; pp.surface_region=&r; pp.output_region=&r;
  pp.surface_color_standard=VAProcColorStandardSRGB; pp.input_color_properties.color_range=VA_SOURCE_RANGE_FULL;
  pp.output_color_standard=VAProcColorStandardBT709; pp.output_color_properties.color_range=VA_SOURCE_RANGE_REDUCED;
  VABufferID bid; VAStatus s;
  if((s=vaCreateBuffer(va,vactx,VAProcPipelineParameterBufferType,sizeof pp,1,&pp,&bid))) return s;
  if((s=vaBeginPicture(va,vactx,nv12))) goto o;
  if((s=vaRenderPicture(va,vactx,&bid,1))) goto o;
  if((s=vaEndPicture(va,vactx))) goto o;
  if(sync) s=vaSyncSurface(va,nv12);
o: vaDestroyBuffer(va,bid); return s;
}

// read Y/U/V at (x,y) and FNV hash of Y+UV planes for region w*h
static uint64_t nv12_read(int npts, const int (*pts)[2], int (*res)[3], int w, int h){
  VAImage img; if(vaDeriveImage(va,nv12,&img)){ printf("vaDeriveImage failed\n"); return 0; }
  unsigned char *p; vaMapBuffer(va,img.buf,(void**)&p);
  for(int i=0;i<npts;i++){ int x=pts[i][0],y=pts[i][1];
    res[i][0]=p[img.offsets[0]+y*img.pitches[0]+x];
    unsigned char *uv=p+img.offsets[1]+(y/2)*img.pitches[1]+(x/2)*2; res[i][1]=uv[0]; res[i][2]=uv[1]; }
  uint64_t hsh=1469598103934665603ull;
  for(int y=0;y<h;y++){ unsigned char*r=p+img.offsets[0]+y*img.pitches[0]; for(int x=0;x<w;x++){hsh^=r[x];hsh*=1099511628211ull;} }
  for(int y=0;y<h/2;y++){ unsigned char*r=p+img.offsets[1]+y*img.pitches[1]; for(int x=0;x<w;x++){hsh^=r[x];hsh*=1099511628211ull;} }
  vaUnmapBuffer(va,img.buf); vaDestroyImage(va,img.image_id); return hsh;
}

struct vafc { uint32_t fc; const char *n; };

// Try importing with a list of VA fourccs; on success keep surface in b. Prints each attempt.
static int import_any(struct buf *b, const struct vafc *cands, int n){
  for(int i=0;i<n;i++){
    VASurfaceID s; double c0=cpu_ms(), t0=now_ms(); VAStatus st=va_import(b,cands[i].fc,&s); double t1=now_ms(), c1=cpu_ms();
    printf("  VA import %s (DRM %s mod %s) as VA_FOURCC_%s: %s (%d) [%.3f ms wall, %.3f ms CPU]",b->label,fourcc_str(b->fmt),mod_name(b->mod),cands[i].n,vaErrorStr(st),st,t1-t0,c1-c0);
    if(st==VA_STATUS_SUCCESS){
      VAImage img; if(!vaDeriveImage(va,s,&img)){ printf(" derived image fourcc=%s",fourcc_str(img.format.fourcc)); vaDestroyImage(va,img.image_id);} else printf(" (derive fails)");
      // VPP sanity: does it render?
      VAStatus vs=vpp(s,b->w,b->h,1); printf(" VPP:%s",vaErrorStr(vs));
      printf("\n");
      if(b->va_ok){ vaDestroySurfaces(va,&s,1); continue; }
      if(vs==VA_STATUS_SUCCESS){ b->vasurf=s; b->va_ok=1; b->va_fourcc=cands[i].fc; } else vaDestroySurfaces(va,&s,1);
    } else printf("\n");
  }
  return b->va_ok?0:-1;
}

static const struct vafc cands_xrgb[]={{VA_FOURCC_BGRX,"BGRX"},{VA_FOURCC_XRGB,"XRGB"},{VA_FOURCC_BGRA,"BGRA"},{VA_FOURCC_ARGB,"ARGB"}};
static const struct vafc cands_argb[]={{VA_FOURCC_BGRA,"BGRA"},{VA_FOURCC_ARGB,"ARGB"},{VA_FOURCC_BGRX,"BGRX"}};
static const struct vafc cands_xbgr[]={{VA_FOURCC_RGBX,"RGBX"},{VA_FOURCC_XBGR,"XBGR"},{VA_FOURCC_RGBA,"RGBA"}};
static const struct vafc *cands_for(uint32_t fmt,int *n){
  if(fmt==DRM_FORMAT_XRGB8888){*n=4;return cands_xrgb;} if(fmt==DRM_FORMAT_ARGB8888){*n=3;return cands_argb;} *n=3;return cands_xbgr; }


// CPU cross-check: gbm_bo_map(READ) (Mesa blits/decompresses tiled/CCS to a linear staging buffer) and print BGRX at points
static void gbm_readback(struct buf *b, int n, const int (*pts)[2]){
  uint32_t stride; void *md=NULL; unsigned char *p=gbm_bo_map(b->bo,0,0,b->w,b->h,GBM_BO_TRANSFER_READ,&stride,&md);
  if(!p){ printf("  gbm_bo_map(READ) failed errno=%d\n",errno); return; }
  printf("  gbm_bo_map(READ) cross-check, bytes B,G,R,X:");
  for(int i=0;i<n;i++){ unsigned char *q=p+(size_t)pts[i][1]*stride+pts[i][0]*4; printf(" (%d,%d)=%d,%d,%d,%d",pts[i][0],pts[i][1],q[0],q[1],q[2],q[3]); }
  printf("\n"); gbm_bo_unmap(b->bo,md);
}

// ---------------------------------------------------------------- synthetic fill
// fill top half red, bottom half blue (or swapped if 'swap'), honouring byte order of the DRM format
static int fill_synth(struct buf *b, int swap){
  uint32_t stride; void *mapd=NULL;
  double t0=now_ms();
  unsigned char *p=gbm_bo_map(b->bo,0,0,b->w,b->h,GBM_BO_TRANSFER_WRITE,&stride,&mapd);
  if(!p){ printf("  gbm_bo_map(WRITE) on %s FAILED errno=%d (%s)\n",b->label,errno,strerror(errno)); return -1; }
  unsigned char red[4],blue[4];
  if(b->fmt==DRM_FORMAT_XBGR8888||b->fmt==DRM_FORMAT_ABGR8888){ unsigned char r[4]={255,0,0,255},bl[4]={0,0,255,255}; memcpy(red,r,4); memcpy(blue,bl,4); }
  else { unsigned char r[4]={0,0,255,255},bl[4]={255,0,0,255}; memcpy(red,r,4); memcpy(blue,bl,4); } // XRGB8888 LE bytes: B,G,R,X
  for(int y=0;y<b->h;y++){ unsigned char *row=p+(size_t)y*stride; const unsigned char *c=((y<b->h/2)^swap)?red:blue;
    for(int x=0;x<b->w;x++) memcpy(row+x*4,c,4); }
  gbm_bo_unmap(b->bo,mapd);
  printf("  gbm_bo_map(WRITE)+fill+unmap on %s ok (map stride=%u, %.2f ms)\n",b->label,stride,now_ms()-t0);
  return 0;
}

static int check(int got[3], int y,int u,int v){ return abs(got[0]-y)<=2&&abs(got[1]-u)<=2&&abs(got[2]-v)<=2; }

static void synth_test(uint32_t fmt, const uint64_t *mods, int nmods, const char *label){
  struct buf b; if(alloc_buf(&b,VW,VH,fmt,mods,nmods,label)){ SUM("synthetic %-30s %s: gbm alloc FAILED\n",label,fourcc_str(fmt)); return; }
  if(fill_synth(&b,0)){ SUM("synthetic %-30s %s mod=%s: gbm_bo_map FAILED\n",label,fourcc_str(fmt),mod_name(b.mod)); gbm_bo_destroy(b.bo); return; }
  int n; const struct vafc *c=cands_for(fmt,&n);
  if(import_any(&b,c,n)){ SUM("synthetic %-30s %s mod=%s: VA import/VPP FAILED\n",label,fourcc_str(fmt),mod_name(b.mod)); gbm_bo_destroy(b.bo); return; }
  int pts[2][2]={{VW/2,VH/4},{VW/2,3*VH/4}}; int res[2][3];
  gbm_readback(&b,2,pts);
  vpp(b.vasurf,VW,VH,1); uint64_t h1=nv12_read(2,pts,res,VW,VH);
  int ok=check(res[0],63,102,240)&&check(res[1],32,240,118);
  printf("  synthetic VPP: top=%d,%d,%d (want 63,102,240) bottom=%d,%d,%d (want 32,240,118) -> %s\n",res[0][0],res[0][1],res[0][2],res[1][0],res[1][1],res[1][2],ok?"PASS":"FAIL");
  // aliasing check: rewrite the bo via CPU (swapped halves), VPP from the SAME cached VA surface
  fill_synth(&b,1); vpp(b.vasurf,VW,VH,1); int res2[2][3]; uint64_t h2=nv12_read(2,pts,res2,VW,VH);
  int ok2=check(res2[0],32,240,118)&&check(res2[1],63,102,240);
  printf("  cached-surface aliasing: after CPU rewrite top=%d,%d,%d bottom=%d,%d,%d hash %016llx -> %016llx -> %s\n",res2[0][0],res2[0][1],res2[0][2],res2[1][0],res2[1][1],res2[1][2],
         (unsigned long long)h1,(unsigned long long)h2,ok2?"PASS (surface aliases bo memory)":"FAIL");
  char vafcs[8]; snprintf(vafcs,sizeof vafcs,"%s",fourcc_str(b.va_fourcc));
  SUM("synthetic %-30s DRM %s mod=%-22s VA %s: red %d,%d,%d blue %d,%d,%d %s; reuse-after-rewrite %s\n",label,fourcc_str(fmt),mod_name(b.mod),vafcs,
      res[0][0],res[0][1],res[0][2],res[1][0],res[1][1],res[1][2],ok?"PASS":"FAIL",ok2?"PASS":"FAIL");
  vaDestroySurfaces(va,&b.vasurf,1); gbm_bo_destroy(b.bo);
}

// ---------------------------------------------------------------- main
int main(int argc, char **argv){
  setvbuf(stdout,NULL,_IOLBF,0);
  printf("=== struct layout (x86_64) for hand-written bindings ===\n");
  printf("sizeof(VADRMPRIMESurfaceDescriptor)=%zu\n",sizeof(VADRMPRIMESurfaceDescriptor));
#define OFF(f) printf("  offsetof(%s)=%zu\n",#f,offsetof(VADRMPRIMESurfaceDescriptor,f))
  OFF(fourcc);OFF(width);OFF(height);OFF(num_objects);OFF(objects);OFF(objects[0].fd);OFF(objects[0].size);OFF(objects[0].drm_format_modifier);OFF(objects[1]);
  OFF(num_layers);OFF(layers);OFF(layers[0].drm_format);OFF(layers[0].num_planes);OFF(layers[0].object_index);OFF(layers[0].offset);OFF(layers[0].pitch);OFF(layers[1]);
  printf("  sizeof(objects[0])=%zu sizeof(layers[0])=%zu\n",sizeof(((VADRMPRIMESurfaceDescriptor*)0)->objects[0]),sizeof(((VADRMPRIMESurfaceDescriptor*)0)->layers[0]));
  printf("sizeof(VASurfaceAttrib)=%zu offsetof(type)=%zu flags=%zu value=%zu; sizeof(VAGenericValue)=%zu value.type@%zu value.value@%zu\n",
    sizeof(VASurfaceAttrib),offsetof(VASurfaceAttrib,type),offsetof(VASurfaceAttrib,flags),offsetof(VASurfaceAttrib,value),sizeof(VAGenericValue),offsetof(VAGenericValue,type),offsetof(VAGenericValue,value));
  printf("VASurfaceAttribMemoryType=%d VASurfaceAttribExternalBufferDescriptor=%d VA_SURFACE_ATTRIB_SETTABLE=0x%x VAGenericValueTypeInteger=%d VAGenericValueTypePointer=%d\n",
    VASurfaceAttribMemoryType,VASurfaceAttribExternalBufferDescriptor,VA_SURFACE_ATTRIB_SETTABLE,VAGenericValueTypeInteger,VAGenericValueTypePointer);
  printf("VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2=0x%08x VA_RT_FORMAT_RGB32=0x%08x VA_FOURCC_BGRX=0x%08x VA_FOURCC_XRGB=0x%08x VA_FOURCC_RGBX=0x%08x VA_FOURCC_BGRA=0x%08x\n",
    VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,VA_RT_FORMAT_RGB32,VA_FOURCC_BGRX,VA_FOURCC_XRGB,VA_FOURCC_RGBX,VA_FOURCC_BGRA);
  printf("DRM_FORMAT_XRGB8888=0x%08x DRM_FORMAT_ARGB8888=0x%08x DRM_FORMAT_XBGR8888=0x%08x GBM_BO_USE_RENDERING=0x%x GBM_BO_TRANSFER_WRITE=0x%x\n",
    DRM_FORMAT_XRGB8888,DRM_FORMAT_ARGB8888,DRM_FORMAT_XBGR8888,GBM_BO_USE_RENDERING,GBM_BO_TRANSFER_WRITE);
  printf("DMA_BUF_IOCTL_EXPORT_SYNC_FILE=0x%lx sizeof(struct dma_buf_export_sync_file)=%zu DMA_BUF_SYNC_READ=%d\n\n",(unsigned long)DMA_BUF_IOCTL_EXPORT_SYNC_FILE,sizeof(struct dma_buf_export_sync_file),DMA_BUF_SYNC_READ);

  dpy=wl_display_connect(NULL); if(!dpy){ fprintf(stderr,"no wayland display\n"); return 1; }
  struct wl_registry *reg=wl_display_get_registry(dpy); wl_registry_add_listener(reg,&reg_l,NULL);
  wl_display_roundtrip(dpy); wl_display_roundtrip(dpy);
  if(!srcmgr||!capmgr||!dmabuf){ fprintf(stderr,"missing globals src=%p cap=%p dmabuf=%p\n",(void*)srcmgr,(void*)capmgr,(void*)dmabuf); return 1; }
  struct out *O=&outs[0];
  for(int i=0;i<nouts;i++){ printf("wl_output %d: %s %dx%d\n",i,outs[i].name,outs[i].w,outs[i].h); if(!strcmp(outs[i].name,"eDP-1")) O=&outs[i]; }
  printf("using output %s\n",O->name);
  printf("zwp_linux_dmabuf_v1 (v%u) global XRGB8888 modifiers:",dmabuf_ver); for(int i=0;i<ndmabuf_glob_mods;i++) printf(" 0x%llx(%s)",(unsigned long long)dmabuf_glob_mods[i],mod_name(dmabuf_glob_mods[i])); printf("\n\n");

  // ---- Q1: session events
  struct sess S0,S1;
  printf("=== Q1: session on %s, options=0 ===\n",O->name); open_session(&S0,O->o,0);
  printf("event order: %s\n",S0.order);
  printf("\n=== Q1: session on %s, options=paint_cursors ===\n",O->name); open_session(&S1,O->o,EXT_IMAGE_COPY_CAPTURE_MANAGER_V1_OPTIONS_PAINT_CURSORS);
  printf("event order: %s\n",S1.order);
  int same = S0.w==S1.w && S0.h==S1.h && S0.nfmt==S1.nfmt && S0.nshm==S1.nshm;
  printf("paint_cursors session identical to options=0 session (size/#fmts/#shm): %s\n",same?"yes":"NO");
  ext_image_copy_capture_session_v1_destroy(S1.s); ext_image_capture_source_v1_destroy(S1.src);
  if(S0.have_dev) map_dev(S0.dev);
  if(!render_node[0]) strcpy(render_node,"/dev/dri/renderD128");
  SUM("Q1 events (%s): %s\n",O->name,S0.order);
  SUM("   size %dx%d, %d shm fmts, dmabuf_device %u:%u -> %s, %d dmabuf fmts\n",S0.w,S0.h,S0.nshm,major(S0.dev),minor(S0.dev),render_node,S0.nfmt);
  for(int i=0;i<S0.nfmt;i++){ SUM("   %s:",fourcc_str(S0.fmt[i])); for(int j=0;j<S0.nmods[i];j++) SUM(" 0x%llx", (unsigned long long)S0.mods[i][j]); SUM("\n"); }

  // pick format
  int fi=-1; for(int i=0;i<S0.nfmt;i++) if(S0.fmt[i]==DRM_FORMAT_XRGB8888) fi=i;
  if(fi<0) for(int i=0;i<S0.nfmt;i++) if(S0.fmt[i]==DRM_FORMAT_ARGB8888) fi=i;
  if(fi<0){ printf("no XRGB/ARGB dmabuf format offered!\n"); return 1; }
  uint32_t FMT=S0.fmt[fi]; uint64_t *MODS=S0.mods[fi]; int NMODS=S0.nmods[fi];
  int W=S0.w,H=S0.h; VW=W; VH=H;
  printf("\nchosen capture format %s with %d modifiers, size %dx%d\n",fourcc_str(FMT),NMODS,W,H);

  // ---- VA init
  int vfd=open(render_node,O_RDWR); va=vaGetDisplayDRM(vfd); int ma,mi;
  if(vaInitialize(va,&ma,&mi)){ printf("vaInitialize failed\n"); return 1; }
  printf("VA %d.%d driver: %s\n",ma,mi,vaQueryVendorString(va));
  if(vaCreateConfig(va,VAProfileNone,VAEntrypointVideoProc,NULL,0,&vacfg)){ printf("vaCreateConfig VPP failed\n"); return 1; }
  { VASurfaceAttrib a={.type=VASurfaceAttribPixelFormat,.flags=VA_SURFACE_ATTRIB_SETTABLE,.value.type=VAGenericValueTypeInteger,.value.value.i=VA_FOURCC_NV12};
    if(vaCreateSurfaces(va,VA_RT_FORMAT_YUV420,W,H,&nv12,1,&a,1)){ printf("nv12 surface failed\n"); return 1; } }
  { VAStatus s=vaCreateContext(va,vacfg,W,H,VA_PROGRESSIVE,NULL,0,&vactx); printf("vaCreateContext(VPP, no render targets): %s\n",vaErrorStr(s));
    if(s){ s=vaCreateContext(va,vacfg,W,H,VA_PROGRESSIVE,&nv12,1,&vactx); printf("vaCreateContext(VPP, nv12 target): %s\n",vaErrorStr(s)); if(s) return 1; } }

  // query what iHD can import for RGB32
  { unsigned n=0; vaQuerySurfaceAttributes(va,vacfg,NULL,&n); VASurfaceAttrib *a=calloc(n,sizeof *a); vaQuerySurfaceAttributes(va,vacfg,a,&n);
    printf("VPP config surface attribs: pixel formats:"); for(unsigned i=0;i<n;i++){ if(a[i].type==VASurfaceAttribPixelFormat) printf(" %s",fourcc_str(a[i].value.value.i));
      if(a[i].type==VASurfaceAttribMemoryType) printf(" [memtypes=0x%x]",a[i].value.value.i);} printf("\n"); free(a); }

  gbm=gbm_create_device(vfd); printf("gbm backend: %s on %s\n\n",gbm_device_get_backend_name(gbm),render_node);

  // ---- Q2/Q3/Q4/Q5: allocate per strategy, capture, import, VPP
  printf("=== Q2-Q5: allocate / capture / import / VPP ===\n");
  #define MAXB 20
  struct buf B[MAXB]; int nb=0;
  if(!alloc_buf(&B[nb],W,H,FMT,MODS,NMODS,"default(all compositor mods)")) nb++;
  { uint64_t m[64]; int n=0; for(int i=0;i<NMODS;i++) if(MODS[i]!=I915_FORMAT_MOD_4_TILED_LNL_CCS && MODS[i]!=I915_FORMAT_MOD_4_TILED_BMG_CCS) m[n++]=MODS[i];
    if(!alloc_buf(&B[nb],W,H,FMT,m,n,"default-minus-CCS")) nb++; }
  { uint64_t lin=DRM_FORMAT_MOD_LINEAR; if(!alloc_buf(&B[nb],W,H,FMT,&lin,1,"linear-only")) nb++; }
  for(int i=0;i<NMODS && nb<MAXB;i++){ if(MODS[i]==DRM_FORMAT_MOD_INVALID||MODS[i]==DRM_FORMAT_MOD_LINEAR) continue;
    char l[64]; snprintf(l,sizeof l,"only-0x%llx",(unsigned long long)MODS[i]); if(!alloc_buf(&B[nb],W,H,FMT,&MODS[i],1,l)) nb++; }
  int pts[5][2]={{W/2,H/2},{8,8},{W-8,8},{8,H-8},{W-8,H-8}};
  struct buf *reuse=NULL;
  for(int i=0;i<nb;i++){
    struct buf *b=&B[i]; printf("\n-- %s: modifier 0x%llx %s\n",b->label,(unsigned long long)b->mod,mod_name(b->mod));
    char res_s[256]="";
    if(make_wl_buffer(b)){ SUM("capture %-28s mod=%-40s wl_buffer FAILED\n",b->label,mod_name(b->mod)); continue; }
    struct frame fr; double dt=capture(&S0,b,&fr);
    if(dt<0){ SUM("capture %-28s mod=%-40s capture FAILED reason=%s\n",b->label,mod_name(b->mod),fr.done?fail_reason(fr.reason):"timeout"); continue; }
    int n; const struct vafc *c=cands_for(FMT,&n);
    if(import_any(b,c,n)){ SUM("capture %-28s mod=%-40s ready %.1fms, fence:%s; VA import FAILED\n",b->label,mod_name(b->mod),dt,fr.sync_state==1?"unsignalled":"none"); continue; }
    vpp(b->vasurf,W,H,1); int res[5][3]; uint64_t h=nv12_read(5,pts,res,W,H);
    for(int k=0;k<5;k++){ char t[40]; snprintf(t,sizeof t," (%d,%d)=%d,%d,%d",pts[k][0],pts[k][1],res[k][0],res[k][1],res[k][2]); strcat(res_s,t); }
    printf("  VPP NV12 from screen:%s hash=%016llx\n",res_s,(unsigned long long)h);
    int black=1; for(int k=1;k<5;k++) if(res[k][0]!=16||res[k][1]!=128||res[k][2]!=128) black=0;
    SUM("capture %-28s mod=%-40s ready %.1fms (pres->ready %.1fms), fence after ready:%s; VA import OK as %s, VPP %s\n",b->label,mod_name(b->mod),dt,fr.t_pres?fr.t_ready-fr.t_pres:-1,
        fr.sync_state==1?"UNSIGNALLED":fr.sync_state==0?"none/signalled":"ioctl-err",fourcc_str(b->va_fourcc),black?"output ALL BLACK (wrong; see gbm cross-check)":"output matches screen");
    if(!reuse && b->mod!=I915_FORMAT_MOD_4_TILED_LNL_CCS) reuse=b;
    gbm_readback(b,5,pts);
  }

  // ---- Q5 synthetic
  printf("\n=== Q5: synthetic red/blue colour check ===\n");
  { uint64_t lin=DRM_FORMAT_MOD_LINEAR;
    synth_test(DRM_FORMAT_XRGB8888,&lin,1,"linear");
    synth_test(DRM_FORMAT_XRGB8888,MODS,NMODS,"default(compositor mods)");
    for(int i=0;i<NMODS;i++) if(MODS[i]!=DRM_FORMAT_MOD_LINEAR&&MODS[i]!=DRM_FORMAT_MOD_INVALID){ char l[64]; snprintf(l,sizeof l,"only-0x%llx",(unsigned long long)MODS[i]); synth_test(DRM_FORMAT_XRGB8888,&MODS[i],1,l); }
    synth_test(DRM_FORMAT_XBGR8888,&lin,1,"linear");
    synth_test(DRM_FORMAT_XBGR8888,MODS,NMODS,"default(compositor mods)");
  }

  // ---- Q6 timing + reuse across captures
  printf("\n=== Q6: timing and surface reuse across captures ===\n");
  for(int bi=0;bi<nb;bi++){ struct buf *b=&B[bi]; if(!b->va_ok) continue;
    printf("-- %s (%s)\n",b->label,mod_name(b->mod));
    // import cost (create+destroy), N times
    int N=50; double c=0,w=0; for(int i=0;i<N;i++){ VASurfaceID s; double c0=cpu_ms(),t0=now_ms(); VAStatus st=va_import(b,b->va_fourcc,&s); w+=now_ms()-t0; c+=cpu_ms()-c0; if(!st) vaDestroySurfaces(va,&s,1); else { printf("  import #%d failed: %s (%d)\n",i,vaErrorStr(st),st); break; } }
    printf("  fresh import (gbm_bo_get_fd + lseek + vaCreateSurfaces DRM_PRIME_2 + close): %.3f ms wall, %.3f ms CPU per import (N=%d)\n",w/N,c/N,N);
    SUM("timing %-28s fresh import %.3f ms CPU",b->label,c/N);
    // VPP: cached surface, submit+sync and pipelined
    double c0=cpu_ms(),t0=now_ms(); for(int i=0;i<N;i++) vpp(b->vasurf,W,H,1); double t1=now_ms(),c1=cpu_ms();
    printf("  VPP from cached surface, submit+sync: %.3f ms wall, %.3f ms CPU per frame\n",(t1-t0)/N,(c1-c0)/N);
    double c2=cpu_ms(),t2=now_ms(); for(int i=0;i<N;i++) vpp(b->vasurf,W,H,0); double c3=cpu_ms(); vaSyncSurface(va,nv12); double t3=now_ms();
    printf("  VPP submit only (no sync, pipelined): %.3f ms CPU per frame, %.3f ms wall per frame incl. final sync\n",(c3-c2)/N,(t3-t2)/N);
    // cost of a full capture round trip
    double cw=0; int ok=0; for(int i=0;i<10;i++){ struct frame fr; double d=capture(&S0,b,&fr); if(d>=0){cw+=d;ok++;} }
    printf("  capture->ready over %d repeated captures: avg %.2f ms\n",ok,ok?cw/ok:-1);
    SUM(", VPP submit+sync %.3f ms wall / %.3f ms CPU, submit-only %.3f ms CPU; capture->ready avg %.2f ms (%d/10)\n",(t1-t0)/N,(c1-c0)/N,(c3-c2)/N,ok?cw/ok:-1,ok);
  }
  if(reuse){
    printf("\n-- reuse across captures on %s: cached VA surface, CPU overwrite, then capture into the same bo\n",reuse->label);
    int p1[1][2]={{W/2,H/2}}; int r[1][3];
    uint64_t hcap1, hfill, hcap2, hcap3, hfresh;
    struct frame fr; capture(&S0,reuse,&fr); vpp(reuse->vasurf,W,H,1); hcap1=nv12_read(1,p1,r,W,H);
    int filled = reuse->mod==DRM_FORMAT_MOD_LINEAR || 1;
    if(filled && !fill_synth(reuse,0)){ vpp(reuse->vasurf,W,H,1); hfill=nv12_read(1,p1,r,W,H); printf("  after CPU fill red/blue: centre=%d,%d,%d hash=%016llx\n",r[0][0],r[0][1],r[0][2],(unsigned long long)hfill);} else hfill=0;
    capture(&S0,reuse,&fr); vpp(reuse->vasurf,W,H,1); hcap2=nv12_read(1,p1,r,W,H);
    printf("  capture #1 hash=%016llx; CPU-fill hash=%016llx; capture #2 (same bo, cached surface) hash=%016llx\n",(unsigned long long)hcap1,(unsigned long long)hfill,(unsigned long long)hcap2);
    // compare cached-surface output with a fresh import of the same bo contents
    VASurfaceID fs; va_import(reuse,reuse->va_fourcc,&fs); vpp(fs,W,H,1); hfresh=nv12_read(1,p1,r,W,H); vaDestroySurfaces(va,&fs,1);
    printf("  fresh import of same bo after capture #2 hash=%016llx -> %s\n",(unsigned long long)hfresh,hfresh==hcap2?"identical to cached surface output":"DIFFERS");
    printf("  waiting 3 s for screen content (bar clock/cursor) to change...\n"); usleep(3000000);
    capture(&S0,reuse,&fr); vpp(reuse->vasurf,W,H,1); hcap3=nv12_read(1,p1,r,W,H);
    printf("  capture #3 (+3 s) hash=%016llx -> %s\n",(unsigned long long)hcap3,hcap3==hcap2?"identical (screen probably static)":"changed");
    int ok = hfill!=hcap1 && hcap2!=hfill && hfresh==hcap2;
    SUM("reuse (%s): cached VA surface reflects new content after CPU write and after re-capture into same bo: %s; capture 3 s later %s\n",
        reuse->label, ok?"YES":"NO", hcap3==hcap2?"identical (static screen)":"changed");
  }

  // ---- Q7 sync: tightly measure fence state over several captures
  printf("\n=== Q7: implicit fence probe after ready ===\n");
  for(int bi=0;bi<nb;bi++){ struct buf *b=&B[bi]; if(b->wb_state<=0) continue;
    int unsig=0,none=0,err=0; for(int i=0;i<10;i++){ struct frame fr; if(capture(&S0,b,&fr)<0) continue; if(fr.sync_state==1)unsig++; else if(fr.sync_state==0)none++; else err++; }
    // also check write-fence probe (DMA_BUF_SYNC_WRITE waits for all fences incl. readers)
    int w=probe_implicit_fence(b->bo,DMA_BUF_SYNC_WRITE);
    printf("  %s: after ready: unsignalled=%d none=%d err=%d; later SYNC_WRITE probe: %s\n",b->label,unsig,none,err,sync_str(w));
    SUM("sync %-28s: fence at ready unsignalled %d/10, none %d/10, err %d\n",b->label,unsig,none,err);
  }

  printf("\n================ SUMMARY ================\n%s",summary);
  return 0;
}
