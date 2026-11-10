import sys
def rd(p):
    d=open(p,'rb').read()
    parts=d.split(b'\n',3)
    w,h=map(int,parts[1].split())
    return w,h,parts[3]
def crop_diff(a,b,w,box):
    x0,y0,x1,y1=box;n=0;bb=None
    for y in range(y0,y1):
        ra=a[(y*w+x0)*3:(y*w+x1)*3]; rb=b[(y*w+x0)*3:(y*w+x1)*3]
        if ra!=rb:
            for x in range(x1-x0):
                if ra[x*3:x*3+3]!=rb[x*3:x*3+3]:
                    n+=1
                    X,Y=x0+x,y
                    bb=(min(bb[0],X),min(bb[1],Y),max(bb[2],X),max(bb[3],Y)) if bb else (X,Y,X,Y)
    return n,bb
w,h,g=rd('/tmp/gpu-cursor.ppm'); _,_,g2=rd('/tmp/gpu-cursor2.ppm'); _,_,s=rd('/tmp/shm-cursor2.ppm'); _,_,n=rd('/tmp/gpu-nocursor.ppm')
box=(1200-100,800-100,1200+200,800+200)
for name,a,b in [('gpu-cursor vs gpu-nocursor',g,n),('gpu-cursor2 vs gpu-nocursor',g2,n),('shm-cursor vs gpu-nocursor',s,n),('gpu-cursor2 vs shm-cursor2',g2,s),('gpu-cursor vs gpu-cursor2',g,g2)]:
    c,bb=crop_diff(a,b,w,box)
    tot=sum(1 for i in range(0,len(a),3) if a[i:i+3]!=b[i:i+3]) if False else None
    # overall
    diff=0;rows=set()
    for y in range(h):
        if a[y*w*3:(y+1)*w*3]!=b[y*w*3:(y+1)*w*3]:
            rows.add(y)
            ra=a[y*w*3:(y+1)*w*3];rb=b[y*w*3:(y+1)*w*3]
            diff+=sum(1 for x in range(w) if ra[x*3:x*3+3]!=rb[x*3:x*3+3])
    print(f'{name}: cursor-region diff px={c} bbox={bb}; overall identical={100*(1-diff/(w*h)):.4f}% (diff {diff}px, rows {min(rows) if rows else None}-{max(rows) if rows else None})')
