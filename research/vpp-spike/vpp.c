// VPP BGRA->NV12 colour-matrix spike for iHD. gcc vpp.c $(pkg-config --cflags --libs libva libva-drm) -o vpp
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_vpp.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define CK(x) do { VAStatus s_ = (x); if (s_ != VA_STATUS_SUCCESS) { fprintf(stderr, "%s:%d %s -> %s\n", __FILE__, __LINE__, #x, vaErrorStr(s_)); exit(1);} } while (0)
static VADisplay dpy;
static double now(void){struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec*1e3+t.tv_nsec/1e6;}

#define P 256
static const char *names[] = {"red","green","blue","white","black","grey"};
static const unsigned char rgb[6][3] = {{255,0,0},{0,255,0},{0,0,255},{255,255,255},{0,0,0},{128,128,128}};
#define NP 6

static const char *csname(int c){
  switch(c){case VAProcColorStandardNone:return "None";case VAProcColorStandardBT601:return "BT601";case VAProcColorStandardBT709:return "BT709";
  case VAProcColorStandardSRGB:return "SRGB";case VAProcColorStandardSTRGB:return "STRGB";case VAProcColorStandardExplicit:return "Explicit";
  case VAProcColorStandardBT2020:return "BT2020";case VAProcColorStandardSMPTE170M:return "SMPTE170M";case VAProcColorStandardBT470BG:return "BT470BG";
  case VAProcColorStandardXVYCC601:return "XVYCC601";case VAProcColorStandardXVYCC709:return "XVYCC709";default:{static char b[8];sprintf(b,"%d",c);return b;}}
}

static VASurfaceID mksurf(unsigned rt, unsigned fourcc, int w, int h){
  VASurfaceAttrib a={.type=VASurfaceAttribPixelFormat,.flags=VA_SURFACE_ATTRIB_SETTABLE,.value.type=VAGenericValueTypeInteger,.value.value.i=fourcc};
  VASurfaceID s; CK(vaCreateSurfaces(dpy,rt,w,h,&s,1,&a,1)); return s;
}

static void fill_derive(VASurfaceID s, const unsigned char *src, int w, int h){
  VAImage img; CK(vaDeriveImage(dpy,s,&img)); void *p; CK(vaMapBuffer(dpy,img.buf,&p));
  for(int y=0;y<h;y++) memcpy((char*)p+img.offsets[0]+y*img.pitches[0], src+y*w*4, w*4);
  CK(vaUnmapBuffer(dpy,img.buf)); CK(vaDestroyImage(dpy,img.image_id));
}

static void fill_put(VASurfaceID s, unsigned fourcc, const unsigned char *src, int w, int h, VAImage *cache){
  if(cache->image_id==VA_INVALID_ID){ VAImageFormat f={.fourcc=fourcc,.byte_order=VA_LSB_FIRST,.bits_per_pixel=32,.depth=32,
      .red_mask=0x00ff0000,.green_mask=0x0000ff00,.blue_mask=0x000000ff,.alpha_mask=0xff000000}; CK(vaCreateImage(dpy,&f,w,h,cache)); }
  void *p; CK(vaMapBuffer(dpy,cache->buf,&p));
  for(int y=0;y<h;y++) memcpy((char*)p+cache->offsets[0]+y*cache->pitches[0], src+y*w*4, w*4);
  CK(vaUnmapBuffer(dpy,cache->buf));
  CK(vaPutImage(dpy,s,cache->image_id,0,0,w,h,0,0,w,h));
}

static VAContextID ctx; static VAConfigID cfg;

static void run_vpp(VASurfaceID in, VASurfaceID out, int w, int h, int scs, VAProcColorProperties *icp, int ocs, VAProcColorProperties *ocp, int sync){
  VAProcPipelineParameterBuffer pp; memset(&pp,0,sizeof pp);
  VARectangle r={0,0,w,h};
  pp.surface=in; pp.surface_region=&r; pp.output_region=&r;
  pp.surface_color_standard=scs; pp.output_color_standard=ocs;
  if(icp) pp.input_color_properties=*icp; if(ocp) pp.output_color_properties=*ocp;
  VABufferID b; CK(vaCreateBuffer(dpy,ctx,VAProcPipelineParameterBufferType,sizeof pp,1,&pp,&b));
  CK(vaBeginPicture(dpy,ctx,out)); CK(vaRenderPicture(dpy,ctx,&b,1)); CK(vaEndPicture(dpy,ctx));
  if(sync) CK(vaSyncSurface(dpy,out));
  vaDestroyBuffer(dpy,b);
}

static void readback(VASurfaceID out, int w, int h, int res[NP][3]){
  VAImage img; CK(vaDeriveImage(dpy,out,&img)); unsigned char *p; CK(vaMapBuffer(dpy,img.buf,(void**)&p));
  for(int i=0;i<NP;i++){ int cx=i*P+P/2, cy=P/2;
    res[i][0]=p[img.offsets[0]+cy*img.pitches[0]+cx];
    unsigned char *uv=p+img.offsets[1]+(cy/2)*img.pitches[1]+(cx/2)*2; res[i][1]=uv[0]; res[i][2]=uv[1]; }
  CK(vaUnmapBuffer(dpy,img.buf)); CK(vaDestroyImage(dpy,img.image_id));
}

int main(int argc,char**argv){
  int fd=open("/dev/dri/renderD128",O_RDWR); dpy=vaGetDisplayDRM(fd); int ma,mi; CK(vaInitialize(dpy,&ma,&mi));
  printf("driver: %s\n", vaQueryVendorString(dpy));
  CK(vaCreateConfig(dpy,VAProfileNone,VAEntrypointVideoProc,NULL,0,&cfg));
  int W=P*NP,H=P;
  unsigned char *buf=malloc(W*H*4);
  for(int y=0;y<H;y++)for(int x=0;x<W;x++){const unsigned char*c=rgb[x/P];unsigned char*q=buf+(y*W+x)*4;q[0]=c[2];q[1]=c[1];q[2]=c[0];q[3]=255;} // B,G,R,A
  unsigned fourccs[]={VA_FOURCC_BGRA,VA_FOURCC_BGRX}; const char*fn[]={"BGRA","BGRX"};
  VASurfaceID out=mksurf(VA_RT_FORMAT_YUV420,VA_FOURCC_NV12,W,H);
  VASurfaceID ins[2]; for(int k=0;k<2;k++){ins[k]=mksurf(VA_RT_FORMAT_RGB32,fourccs[k],W,H); fill_derive(ins[k],buf,W,H);}
  VASurfaceID all[3]={out,ins[0],ins[1]};
  CK(vaCreateContext(dpy,cfg,W,H,VA_PROGRESSIVE,all,3,&ctx));

  // caps
  VAProcPipelineCaps caps; memset(&caps,0,sizeof caps);
  VAProcColorStandardType ics[32],ocs[32]; for(int i=0;i<32;i++)ics[i]=ocs[i]=(VAProcColorStandardType)0x7777; caps.input_color_standards=ics; caps.num_input_color_standards=32; caps.output_color_standards=ocs; caps.num_output_color_standards=32;
  CK(vaQueryVideoProcPipelineCaps(dpy,ctx,NULL,0,&caps));
  printf("caps num_in=%u num_out=%u\n",caps.num_input_color_standards,caps.num_output_color_standards); printf("caps input color standards:"); for(unsigned i=0;i<caps.num_input_color_standards;i++)printf(" %s",csname(caps.input_color_standards[i]));
  printf("\ncaps output color standards:"); for(unsigned i=0;i<caps.num_output_color_standards;i++)printf(" %s",csname(caps.output_color_standards[i]));
  printf("\ninput pixel formats: %u, output pixel formats: %u\n\n",caps.num_input_pixel_formats,caps.num_output_pixel_formats);

  int scsv[]={VAProcColorStandardSRGB,VAProcColorStandardBT709,VAProcColorStandardNone,VAProcColorStandardExplicit,VAProcColorStandardSTRGB};
  int ocsv[]={VAProcColorStandardBT709,VAProcColorStandardBT601,VAProcColorStandardExplicit,VAProcColorStandardNone};
  int orng[]={VA_SOURCE_RANGE_UNKNOWN,VA_SOURCE_RANGE_REDUCED,VA_SOURCE_RANGE_FULL};
  int irng[]={VA_SOURCE_RANGE_UNKNOWN,VA_SOURCE_RANGE_FULL};
  printf("%-4s %-8s %-4s %-8s %-4s | ","in","surf_cs","irng","out_cs","orng"); for(int i=0;i<NP;i++)printf("%-12s",names[i]); printf(" matrix\n");
  for(int k=0;k<2;k++)for(int a=0;a<5;a++)for(int ir=0;ir<2;ir++)for(int b=0;b<4;b++)for(int c=0;c<3;c++){
    VAProcColorProperties icp={0},ocp={0};
    icp.color_range=irng[ir]; ocp.color_range=orng[c];
    if(scsv[a]==VAProcColorStandardExplicit){icp.colour_primaries=1;icp.transfer_characteristics=13;icp.matrix_coefficients=0;}
    if(ocsv[b]==VAProcColorStandardExplicit){ocp.colour_primaries=1;ocp.transfer_characteristics=1;ocp.matrix_coefficients=1;}
    // clear output
    run_vpp(ins[k],out,W,H,scsv[a],&icp,ocsv[b],&ocp,1);
    int res[NP][3]; readback(out,W,H,res);
    const char*m="?";
    if(res[0][0]==63&&res[0][1]==102&&res[0][2]==240)m="BT709-limited";
    else if(abs(res[0][0]-63)<=1&&abs(res[0][1]-102)<=1&&abs(res[0][2]-240)<=1)m="~BT709-limited";
    else if(abs(res[0][0]-81)<=1&&abs(res[0][1]-90)<=1&&abs(res[0][2]-240)<=1)m="BT601-limited";
    else if(abs(res[0][0]-54)<=1&&abs(res[0][1]-99)<=1&&abs(res[0][2]-255)<=1)m="BT709-full";
    else if(abs(res[0][0]-76)<=1&&abs(res[0][1]-85)<=1&&abs(res[0][2]-255)<=1)m="BT601-full";
    printf("%-4s %-8s %-4d %-8s %-4d | ",fn[k],csname(scsv[a]),irng[ir],csname(ocsv[b]),orng[c]);
    for(int i=0;i<NP;i++){char t[16];sprintf(t,"%d,%d,%d",res[i][0],res[i][1],res[i][2]);printf("%-12s",t);} printf(" %s\n",m);
  }

  { // Explicit: which field does iHD key on?
    int combos[][4]={{1,1,1,1},{2,2,1,1},{2,1,1,1},{1,2,2,1},{6,6,6,1},{1,1,1,2},{9,14,9,1}}; // prim,trc,matrix,range
    printf("\nExplicit output, surf=SRGB/full, BGRX input:\n");
    for(unsigned i=0;i<sizeof combos/sizeof combos[0];i++){
      VAProcColorProperties icp={0},ocp={0}; icp.color_range=2;
      ocp.colour_primaries=combos[i][0];ocp.transfer_characteristics=combos[i][1];ocp.matrix_coefficients=combos[i][2];ocp.color_range=combos[i][3];
      run_vpp(ins[1],out,W,H,VAProcColorStandardSRGB,&icp,VAProcColorStandardExplicit,&ocp,1);
      int res[NP][3]; readback(out,W,H,res);
      printf("prim=%d trc=%d matrix=%d range=%d | red %d,%d,%d green %d,%d,%d blue %d,%d,%d\n",combos[i][0],combos[i][1],combos[i][2],combos[i][3],
        res[0][0],res[0][1],res[0][2],res[1][0],res[1][1],res[1][2],res[2][0],res[2][1],res[2][2]);
    }
  }
  if(argc>1){ // timing at 2388x1668
    int TW=2388,TH=1668;
    VASurfaceID ti=mksurf(VA_RT_FORMAT_RGB32,VA_FOURCC_BGRX,TW,TH), to=mksurf(VA_RT_FORMAT_YUV420,VA_FOURCC_NV12,TW,TH);
    VASurfaceID ts[2]={ti,to}; VAContextID c2; CK(vaCreateContext(dpy,cfg,TW,TH,VA_PROGRESSIVE,ts,2,&c2)); VAContextID save=ctx; ctx=c2;
    unsigned char *big=malloc(TW*TH*4); for(int i=0;i<TW*TH*4;i++)big[i]=i*7;
    VAProcColorProperties icp={0},ocp={0}; ocp.color_range=VA_SOURCE_RANGE_REDUCED; icp.color_range=VA_SOURCE_RANGE_FULL;
    int N=200;
    for(int i=0;i<10;i++){fill_derive(ti,big,TW,TH);}
    double t0=now(); for(int i=0;i<N;i++) fill_derive(ti,big,TW,TH); double t1=now();
    printf("\nupload derive+memcpy (map/copy/unmap): %.3f ms/frame\n",(t1-t0)/N);
    VAImage cache={.image_id=VA_INVALID_ID}; for(int i=0;i<10;i++) fill_put(ti,VA_FOURCC_BGRX,big,TW,TH,&cache);
    t0=now(); for(int i=0;i<N;i++) fill_put(ti,VA_FOURCC_BGRX,big,TW,TH,&cache); t1=now();
    printf("upload vaCreateImage(once)+memcpy+vaPutImage: %.3f ms/frame\n",(t1-t0)/N);
    t0=now(); for(int i=0;i<N;i++){void*p;CK(vaMapBuffer(dpy,cache.buf,&p));memcpy(p,big,TW*TH*4);CK(vaUnmapBuffer(dpy,cache.buf));} t1=now();
    printf("  (memcpy into VAImage buffer only: %.3f ms)\n",(t1-t0)/N);
    t0=now(); for(int i=0;i<N;i++) CK(vaPutImage(dpy,ti,cache.image_id,0,0,TW,TH,0,0,TW,TH)); CK(vaSyncSurface(dpy,ti)); t1=now();
    printf("  (vaPutImage only: %.3f ms)\n",(t1-t0)/N);
    for(int i=0;i<10;i++) run_vpp(ti,to,TW,TH,VAProcColorStandardBT709,&icp,VAProcColorStandardBT709,&ocp,1);
    t0=now(); clock_t c0=clock(); for(int i=0;i<N;i++) run_vpp(ti,to,TW,TH,VAProcColorStandardBT709,&icp,VAProcColorStandardBT709,&ocp,1); t1=now(); clock_t c1=clock();
    printf("VPP convert (submit+sync each): %.3f ms wall/frame, %.3f ms CPU/frame\n",(t1-t0)/N,(double)(c1-c0)*1000/CLOCKS_PER_SEC/N);
    t0=now(); c0=clock(); for(int i=0;i<N;i++) run_vpp(ti,to,TW,TH,VAProcColorStandardBT709,&icp,VAProcColorStandardBT709,&ocp,0); CK(vaSyncSurface(dpy,to)); t1=now(); c1=clock();
    printf("VPP convert (pipelined, one sync): %.3f ms wall/frame, %.3f ms CPU/frame\n",(t1-t0)/N,(double)(c1-c0)*1000/CLOCKS_PER_SEC/N);
    ctx=save;
  }
  return 0;
}
