# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
"""Fetches 24 public aerial tiles (six places, the four z17 children of a z16
tile each) into the directory given: the bench's input. Not committed.
Usage: python3 -I fetch_children.py tiles"""
import math, sys, urllib.request, os
out = sys.argv[1]
places = {"paris":(48.8566,2.3522),"alps":(45.92,6.87),"amazon":(-3.4,-62.2),"sahara":(23.4,12.0),"coast":(43.21,5.37),"farm":(48.3,1.6)}
def qk(z,x,y):
    s=""
    for i in range(z,0,-1):
        d=0; m=1<<(i-1)
        if x&m: d+=1
        if y&m: d+=2
        s+=str(d)
    return s
z=16
for name,(lat,lon) in places.items():
    n=2**z; x=int((lon+180)/360*n); y=int((1-math.asinh(math.tan(math.radians(lat)))/math.pi)/2*n)
    for i,(cx,cy) in enumerate([(2*x,2*y),(2*x+1,2*y),(2*x,2*y+1),(2*x+1,2*y+1)]):
        url=f"https://ecn.t{i}.tiles.virtualearth.net/tiles/a{qk(z+1,cx,cy)}.jpeg?g=1"
        data=urllib.request.urlopen(urllib.request.Request(url,headers={"User-Agent":"bench"})).read()
        open(os.path.join(out,f"{name}-{i}.jpg"),"wb").write(data); print(name,i,len(data))
