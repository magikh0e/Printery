#!/usr/bin/env python3
"""Generate a set of head sizing rings as binary STLs.

You try them on and the one that just fits tells you your size. That number is
what a helmet or a hat model wants from you, and it beats a tape measure held
by yourself in a mirror, which is how most people get it wrong.

The cross section is not a circle. Heads are ovals, flatter at the forehead
than at the back, so a circular ring lies to you: it binds at the temples and
gapes front to back. These use the same superellipse Headfit builds its heads
from, with the same exponents, so a ring that fits means the number it carries
is the number to type into the tool.

Each ring is labelled on its top face, because an unlabelled set of rings is a
bag of identical rings within about a day. Digits are raised on a pad at the
front, printed flat with no supports.

    python tools/make-rings.py            520 to 640 mm in 10 mm steps
    python tools/make-rings.py 540 600 5  a range and step of your own
"""

import math, os, struct, sys

# Matching Headfit: flatter forehead, rounder occiput.
NF, NB = 2.32, 2.12
# Its default head, used only for its proportions. Every ring is this shape
# scaled until the inner perimeter is the size on the label, so the set varies
# in size and not in character.
REF_L, REF_W = 196.0, 152.0

WALL   = 3.0    # thick enough to hold its shape, thin enough to flex on
HEIGHT = 10.0   # a band, not a crown
# The bottom edges are chamfered, and that is structural rather than tidy.
# A 3 x 10 band resists being levered up off the bed about eleven times harder
# than it resists being bent round a head, and levering it loads the layer
# lines in peel, which is the one thing they are bad at. A 570 ring glues
# itself to the plate over 2200 mm2 and all of that has to let go through the
# single point a scraper gets under. The first one printed snapped on removal.
# Chamfering takes the contact strip from 3.0 mm to 1.4 mm, a little over half
# the bond, and leaves a ramp a scraper slides along instead of wedging under.
# It eats the first 0.8 mm of height only, so the wall the size is measured on
# is untouched.
CHAMFER = 0.8
# The label needs a wider top face than a 3 mm band gives, so the wall
# thickens outwards under it. Outwards matters: this used to be a separate
# block sitting 10 mm inboard of the sizing wall, which meant the pad met your
# forehead before the wall did and every ring read small at the front. Ramped
# in and out over PAD_RAMP rather than butted on, because a stiff block on a
# thin band puts the whole bending load into the two joints, and the first one
# printed broke at exactly that line taking it off the plate.
PAD_W, PAD_RAMP, PAD_EXTRA = 38.0, 16.0, 7.0
DIGIT_H, DIGIT_W, SEG, RAISE = 7.0, 4.4, 1.0, 0.7
SEGMENTS = {
    '0':'abcdef', '1':'bc', '2':'abged', '3':'abgcd', '4':'fgbc',
    '5':'afgcd', '6':'afgecd', '7':'abc', '8':'abcdefg', '9':'abfgcd',
}

def ring_xz(th, fz, bz, hw):
    s, c = math.sin(th), math.cos(th)
    b = fz if c >= 0 else bz
    n = NF if c >= 0 else NB
    x = hw * math.copysign(abs(s) ** (2 / n), s or 1.0)
    z = b * math.copysign(abs(c) ** (2 / n), c or 1.0)
    return x, z

def perimeter(hw, fz, bz, n=512):
    p = px = pz = 0.0
    for i in range(n + 1):
        x, z = ring_xz(i / n * math.tau, fz, bz, hw)
        if i:
            p += math.hypot(x - px, z - pz)
        px, pz = x, z
    return p

def shape_for(circumference):
    """The reference oval scaled so its perimeter is the size on the label."""
    hw, fz, bz = REF_W / 2, REF_L * 0.465, REF_L * 0.535
    k = circumference / perimeter(hw, fz, bz)
    return hw * k, fz * k, bz * k

def quad(tris, a, b, c, d):
    tris.append((a, b, c))
    tris.append((a, c, d))

def pad_extra(d):
    """Extra wall thickness at arc distance d from the front of the ring."""
    if d >= PAD_W / 2 + PAD_RAMP:
        return 0.0
    if d <= PAD_W / 2:
        return PAD_EXTRA
    t = (d - PAD_W / 2) / PAD_RAMP
    return PAD_EXTRA * (0.5 + 0.5 * math.cos(math.pi * t))

def prism(tris, rings):
    """A stack of matching rings of points, capped top and bottom."""
    for lo, hi in zip(rings, rings[1:]):
        for k in range(len(lo)):
            l = (k + 1) % len(lo)
            quad(tris, lo[k], hi[k], hi[l], lo[l])
    bot, top = rings[0], rings[-1]
    for k in range(len(bot) - 2):
        tris.append((bot[0], bot[k+1], bot[k+2]))          # faces down
        tris.append((top[0], top[k+2], top[k+1]))          # faces up

def chamfered_box(tris, x0, x1, y0, y1, z0, z1, ch):
    """A box sitting on the bed with its bottom edges pulled in, for the same
    reason the band's are."""
    rect = lambda a, b, c, d, y: [(a,y,c), (b,y,c), (b,y,d), (a,y,d)]
    prism(tris, [rect(x0+ch, x1-ch, z0+ch, z1-ch, y0),
                 rect(x0, x1, z0, z1, y0+ch),
                 rect(x0, x1, z0, z1, y1)])

def box(tris, x0, x1, y0, y1, z0, z1):
    p = [(x0,y0,z0),(x1,y0,z0),(x1,y1,z0),(x0,y1,z0),
         (x0,y0,z1),(x1,y0,z1),(x1,y1,z1),(x0,y1,z1)]
    quad(tris, p[0],p[3],p[2],p[1])   # bottom
    quad(tris, p[4],p[5],p[6],p[7])   # top
    quad(tris, p[0],p[1],p[5],p[4])
    quad(tris, p[1],p[2],p[6],p[5])
    quad(tris, p[2],p[3],p[7],p[6])
    quad(tris, p[3],p[0],p[4],p[7])

def digit(tris, ch, ox, oz, top):
    """Seven segments, raised on the pad. Chunky on purpose: this gets read at
    arm's length in a workshop, not admired."""
    w, h, s = DIGIT_W, DIGIT_H, SEG
    mid = h / 2 - s / 2
    place = {
        'a': (0, w, h - s, h), 'g': (0, w, mid, mid + s), 'd': (0, w, 0, s),
        'f': (0, s, mid, h),   'b': (w - s, w, mid, h),
        'e': (0, s, 0, mid + s), 'c': (w - s, w, 0, mid + s),
    }
    # Neighbouring segments meet corner to corner, which leaves a mesh that is
    # closed but not manifold: it prints fine and still gets reported as a fault
    # by every checker someone runs it through. Growing every segment equally
    # does not help, because two corners that coincided still coincide. Each is
    # grown along its own long axis instead, so the bars overlap through each
    # other and no two corners land on the same point. Far below a nozzle width.
    e = 0.02
    for seg in SEGMENTS.get(ch, ''):
        x0, x1, z0, z1 = place[seg]
        wide = (x1 - x0) > (z1 - z0)
        gx = e if wide else 0.0
        gz = 0.0 if wide else e
        # Sunk 0.2 mm into the pad rather than set exactly on it. Resting a
        # separate solid on a face it shares exactly leaves the two touching
        # and not overlapping, which is a union a slicer has to be lucky to
        # get right. Overlapping costs nothing and asks nobody for luck.
        box(tris, ox + x0 - gx, ox + x1 + gx, top - 0.2, top + RAISE,
                  oz + z0 - gz, oz + z1 + gz)

def at(p, off_y):
    """A profile point, offset radially outwards from a point on the oval."""
    x, z = p
    d = math.hypot(x, z) or 1.0
    off, y = off_y
    return (x + x / d * off, y, z + z / d * off)

def build(circumference, steps=320):
    hw, fz, bz = shape_for(circumference)
    tris = []
    ring = [ring_xz(i / steps * math.tau, fz, bz, hw) for i in range(steps)]

    # Arc distance from the front, going whichever way round is shorter, so the
    # pad ramp is symmetric about the label.
    seg = [math.hypot(ring[(i+1) % steps][0] - ring[i][0],
                      ring[(i+1) % steps][1] - ring[i][1]) for i in range(steps)]
    total, run, dist = sum(seg), 0.0, []
    for i in range(steps):
        dist.append(min(run, total - run))
        run += seg[i]
    wall = [WALL + pad_extra(d) for d in dist]

    # One closed profile swept round the oval, thickening outwards where the
    # label sits. Offsets are measured from the inner face, so that face is the
    # same oval the whole way round and nothing here changes what the ring
    # measures.
    prof = lambda w: [(CHAMFER, 0), (0, CHAMFER), (0, HEIGHT),
                      (w, HEIGHT), (w, CHAMFER), (w - CHAMFER, 0)]
    for i in range(steps):
        j = (i + 1) % steps
        a, b = prof(wall[i]), prof(wall[j])
        for k in range(len(a)):
            l = (k + 1) % len(a)
            quad(tris, at(ring[i], a[k]), at(ring[i], a[l]),
                       at(ring[j], b[l]), at(ring[j], b[k]))

    # Digits centred across the widened top face at the front.
    front = ring[0][1]
    label = str(int(round(circumference)))
    span = len(label) * DIGIT_W + (len(label) - 1) * 1.6
    x = -span / 2
    for ch in label:
        digit(tris, ch, x, front + wall[0] / 2 - DIGIT_H / 2, HEIGHT)
        x += DIGIT_W + 1.6

    # Everything above is built with Y up, because an oval in XZ with a height
    # in Y is the way the maths reads. An STL for printing is Z up, and the
    # first thirteen of these went out the other way: a slicer opened them as a
    # 213 mm tall part balanced on its rim, needing supports it should never
    # need. A quarter turn about X on the way out puts the band flat in XY with
    # the label towards the back of the bed. It is a rotation and not a mirror,
    # so the winding and the digits both survive it.
    return [tuple((p[0], -p[2], p[1]) for p in t) for t in tris]

def write_stl(path, tris):
    with open(path, 'wb') as f:
        f.write(b'head sizing ring, printery.magikh0e.com/headfit.html'.ljust(80, b' '))
        f.write(struct.pack('<I', len(tris)))
        for a, b, c in tris:
            ux, uy, uz = (b[0]-a[0], b[1]-a[1], b[2]-a[2])
            vx, vy, vz = (c[0]-a[0], c[1]-a[1], c[2]-a[2])
            nx, ny, nz = uy*vz-uz*vy, uz*vx-ux*vz, ux*vy-uy*vx
            n = math.hypot(nx, ny, nz) or 1.0
            f.write(struct.pack('<3f', nx/n, ny/n, nz/n))
            for p in (a, b, c):
                f.write(struct.pack('<3f', *p))
            f.write(b'\0\0')

def main():
    lo  = int(sys.argv[1]) if len(sys.argv) > 1 else 520
    hi  = int(sys.argv[2]) if len(sys.argv) > 2 else 640
    inc = int(sys.argv[3]) if len(sys.argv) > 3 else 10
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'rings')
    out = os.path.normpath(out)
    os.makedirs(out, exist_ok=True)
    made = []
    for c in range(lo, hi + 1, inc):
        tris = build(c)
        name = 'head-ring-%dmm.stl' % c
        write_stl(os.path.join(out, name), tris)
        hw, fz, bz = shape_for(c)
        made.append((name, len(tris), perimeter(hw, fz, bz)))
    for name, n, p in made:
        print('  %-24s %5d triangles   inner %.1f mm' % (name, n, p))
    print('\n  %d rings in %s' % (len(made), out))

if __name__ == '__main__':
    main()
