#!/usr/bin/env python3
"""Builds the RBS icon paths from Tabler Icons (outline, MIT, ADR-0094).

Usage: pip install svgelements; python3 tools/icons/convert_tabler.py out.json
Fetches Tabler 3.26.0 SVGs, converts them to absolute M/L/C/Z path commands scaled
from the 24 grid to the 16 x 16 view box, and writes {rbs name: path} to out.json.
Paste the paths into crates/rbs-ui/src/icon.rs, then regenerate the Slint copy with
`RBS_UPDATE_MENU_BAR=1 cargo test -p rbs-app --bin rbs icons`.
"""
import re,sys,json,urllib.request,math
from svgelements import Path, Move, Line, CubicBezier, QuadraticBezier, Arc, Close, Matrix, SVG
MAP={
"more":"dots","chevron-down":"chevron-down","chevron-up":"chevron-up","chevron-left":"chevron-left","chevron-right":"chevron-right",
"check":"check","close":"x","plus":"plus","pin":"pin","panel-left":"layout-sidebar","panel-right":"layout-sidebar-right","panel-bottom":"layout-bottombar",
"bolt":"bolt","grid":"layout-grid","copy":"copy","window":"app-window","search":"search","monitor":"device-desktop","camera":"video","mic":"microphone",
"speaker":"volume","eye":"eye","eye-off":"eye-off","expand":"arrows-maximize","lock":"lock","unlock":"lock-open","speaker-off":"volume-off",
"sliders":"adjustments-horizontal","palette":"palette","keyboard":"keyboard","broadcast":"broadcast","link":"link","globe":"world",
"warning":"alert-triangle","info":"info-circle","gear":"settings","user":"user","folder":"folder","undo":"arrow-back-up","redo":"arrow-forward-up",
"layout":"layout-2","panel-all":"layout-board-split","panel-none":"layout-off","reset":"restore","refresh":"refresh","image":"photo","pencil":"pencil",
"trash":"trash","transition":"transition-right","export":"upload","import":"download","arrow-left":"arrow-left","arrow-right":"arrow-right","menu":"menu-2",
"flip-h":"flip-vertical","flip-v":"flip-horizontal","rotate-cw":"rotate-clockwise","rotate-ccw":"rotate-clockwise","fit":"aspect-ratio","fill-canvas":"maximize",
"stretch":"arrows-horizontal","center":"focus-centered",
"collapse":"chevrons-up","auto-hide":"layout-sidebar-left-collapse","move":"arrows-move","float":"picture-in-picture","dock":"picture-in-picture-off",
}
S=16/24
# Tabler names flip icons by the axis the picture is mirrored ACROSS in the opposite sense to RBS:
# RBS flip-h (left-right mirror, vertical axis) is Tabler flip-vertical, and the reverse.
MIRROR={'rotate-ccw'}  # counter-clockwise = mirrored clockwise icon
def f(x): 
    s=f"{x*S:.2f}".rstrip('0').rstrip('.'); return s if s!="-0" else "0"
out={}
for ours,tb in MAP.items():
    svg=urllib.request.urlopen(f"https://unpkg.com/@tabler/icons@3.26.0/icons/outline/{tb}.svg",timeout=20).read().decode()
    body=re.search(r'<svg[^>]*>(.*)</svg>',svg,re.S).group(1)
    d=[]
    for el in re.finditer(r'<(path|circle|rect|line|polyline|polygon|ellipse)\s([^>]*?)/?>',body):
        tag,attrs=el.group(1),dict(re.findall(r'([\w-]+)="([^"]*)"',el.group(2)))
        if attrs.get('stroke')=='none': continue
        if 'fill' in attrs and attrs['fill'] not in('none',): 
            print("FILLED",ours,tb,attrs); 
        if tag=='path': d.append(attrs['d'])
        elif tag=='circle':
            cx,cy,r=float(attrs['cx']),float(attrs['cy']),float(attrs['r']); d.append(f"M{cx-r} {cy}a{r} {r} 0 1 0 {2*r} 0a{r} {r} 0 1 0 {-2*r} 0Z")
        elif tag=='ellipse':
            cx,cy,rx,ry=[float(attrs[k]) for k in('cx','cy','rx','ry')]; d.append(f"M{cx-rx} {cy}a{rx} {ry} 0 1 0 {2*rx} 0a{rx} {ry} 0 1 0 {-2*rx} 0Z")
        elif tag=='rect':
            x,y,w,h=[float(attrs.get(k,0)) for k in('x','y','width','height')]; rx=float(attrs.get('rx',0))
            if rx: d.append(f"M{x+rx} {y}h{w-2*rx}a{rx} {rx} 0 0 1 {rx} {rx}v{h-2*rx}a{rx} {rx} 0 0 1 {-rx} {rx}h{-(w-2*rx)}a{rx} {rx} 0 0 1 {-rx} {-rx}v{-(h-2*rx)}a{rx} {rx} 0 0 1 {rx} {-rx}Z")
            else: d.append(f"M{x} {y}h{w}v{h}h{-w}Z")
        elif tag=='line': d.append(f"M{attrs['x1']} {attrs['y1']}L{attrs['x2']} {attrs['y2']}")
        else:
            pts=re.findall(r'-?[\d.]+',attrs['points']); pp=[(pts[i],pts[i+1]) for i in range(0,len(pts),2)]
            d.append("M"+" L".join(f"{a} {b}" for a,b in pp)+("Z" if tag=='polygon' else ""))
    p=Path(" ".join(d))
    if ours in MIRROR: p=p*Matrix('scale(-1,1) translate(-24,0)')
    cmds=[]
    for seg in p:
        if isinstance(seg,Move): cmds.append(f"M {f(seg.end.x)} {f(seg.end.y)}")
        elif isinstance(seg,Close): cmds.append("Z")
        elif isinstance(seg,Line): cmds.append(f"L {f(seg.end.x)} {f(seg.end.y)}")
        elif isinstance(seg,CubicBezier): cmds.append(f"C {f(seg.control1.x)} {f(seg.control1.y)} {f(seg.control2.x)} {f(seg.control2.y)} {f(seg.end.x)} {f(seg.end.y)}")
        elif isinstance(seg,QuadraticBezier):
            # to cubic
            s,c,e=seg.start,seg.control,seg.end
            c1=(s.x+2/3*(c.x-s.x),s.y+2/3*(c.y-s.y)); c2=(e.x+2/3*(c.x-e.x),e.y+2/3*(c.y-e.y))
            cmds.append(f"C {f(c1[0])} {f(c1[1])} {f(c2[0])} {f(c2[1])} {f(e.x)} {f(e.y)}")
        elif isinstance(seg,Arc):
            for cb in seg.as_cubic_curves():
                cmds.append(f"C {f(cb.control1.x)} {f(cb.control1.y)} {f(cb.control2.x)} {f(cb.control2.y)} {f(cb.end.x)} {f(cb.end.y)}")
    out[ours]=" ".join(cmds)
json.dump(out,open(sys.argv[1] if len(sys.argv)>1 else 'tabler16.json','w'),indent=0)
print(len(out),max(len(v) for v in out.values()))
