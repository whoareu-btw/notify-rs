#!/usr/bin/env python3
"""A small Wayland compositor for testing notify-rs without a display.

It speaks the real wire protocol (message framing, SCM_RIGHTS file descriptors,
shared-memory buffers) for just the interfaces notify-rs uses, and is strict
about the things real compositors are strict about: unknown opcodes, wrongly
typed arguments, reusing an object id before delete_id, attaching a buffer to
a layer surface before the first ack_configure, shm buffers that don't fit
their pool, buffer sizes that don't match the scale.

Request signatures come from tests/fixtures/wayland_protocol.txt, so the table
is shared with the Rust opcode check.

Usage: mock_compositor.py PROTOCOL_TABLE [--without a,b,..] [--no-pointer] [--dump DIR]
Environment: XDG_RUNTIME_DIR and WAYLAND_DISPLAY give the socket path.
stdout: one line per event, `name key=value key=a,b ...` (spaces in values become _).
stdin:  one command per line (see COMMANDS below).
"""
import array, json, mmap, os, select, socket, struct, sys

W, H = 1920, 1080   # output size in logical pixels

# ----------------------------------------------------------------- protocol
TABLE = {}  # (interface, 'req'|'evt') -> {opcode: (name, [types])}
VERSIONS = {}


def load_table(path):
    for line in open(path):
        line = line.strip()
        if not line or line.startswith('#'):
            continue
        iface, kind, op, name, sig = line.split('|')
        TABLE.setdefault((iface, kind), {})[int(op)] = (name, sig.split(',') if sig else [])


def fmt(v):
    if v is None:
        return '-'
    if isinstance(v, bool):
        return 'true' if v else 'false'
    if isinstance(v, (list, tuple)):
        return ','.join(fmt(x) for x in v) if v else '-'
    return str(v).replace(' ', '_')


def emit(**kw):
    ev = kw.pop('ev')
    print(ev + ''.join(f' {k}={fmt(v)}' for k, v in kw.items()), flush=True)


class ProtocolError(Exception):
    def __init__(self, obj, code, msg):
        super().__init__(msg)
        self.obj, self.code, self.msg = obj, code, msg


class Obj:
    def __init__(self, iface, version, **kw):
        self.iface, self.version = iface, version
        self.__dict__.update(kw)


class Client:
    def __init__(self, sock):
        self.sock = sock
        self.buf = b''
        self.fds = []
        self.objs = {1: Obj('wl_display', 1)}
        self.next_serial = 1
        self.focus = None     # surface obj
        self.enter_serial = 0
        self.dead = False
        self.max_id = 1
        self.limbo = {}   # id -> time its delete_id will be sent

    def serial(self):
        self.next_serial += 1
        return self.next_serial


def enc_string(s):
    b = s.encode() + b'\0'
    return struct.pack('<I', len(b)) + b + b'\0' * ((4 - len(b) % 4) % 4)


def send_evt(c, oid, name, *args):
    o = c.objs[oid]
    for op, (n, sig) in TABLE[(o.iface, 'evt')].items():
        if n == name:
            break
    else:
        raise KeyError((o.iface, name))
    assert len(sig) == len(args), (o.iface, name, sig, args)
    body = b''
    for t, a in zip(sig, args):
        if t in ('u',) or t.startswith('obj') or t.startswith('new'):
            body += struct.pack('<I', a)
        elif t == 'i':
            body += struct.pack('<i', a)
        elif t == 'fixed':
            body += struct.pack('<i', int(round(a * 256)))
        elif t == 's':
            body += enc_string(a)
        else:
            raise ValueError(t)
    msg = struct.pack('<II', oid, ((8 + len(body)) << 16) | op) + body
    try:
        c.sock.sendall(msg)
    except OSError:
        c.dead = True


def fail(c, obj, code, text):
    emit(ev='protocol_error', code=code, text=text)
    send_evt(c, 1, 'error', obj, code, text)
    c.dead = True


# ----------------------------------------------------------------- state
class Server:
    def __init__(self, without, pointer, dumpdir):
        self.without = without
        self.pointer = pointer
        self.dumpdir = dumpdir
        self.scale120 = 120
        self.clients = []
        self.hold = False
        self.dump_n = 0
        self.globals = []
        names = [('wl_compositor', 6), ('wl_shm', 1), ('wl_seat', 5), ('wl_output', 3),
                 ('zwlr_layer_shell_v1', 4), ('wp_viewporter', 1),
                 ('wp_fractional_scale_manager_v1', 1), ('wp_cursor_shape_manager_v1', 1)]
        short = {'zwlr_layer_shell_v1': 'layer_shell', 'wp_viewporter': 'viewporter',
                 'wp_fractional_scale_manager_v1': 'fractional', 'wp_cursor_shape_manager_v1': 'cursor',
                 'wl_seat': 'seat'}
        for i, (n, v) in enumerate(names, 1):
            if short.get(n, n) not in without:
                self.globals.append((i, n, v))
        self.shm_formats = [0, 1]

    # -- helpers
    def surfaces(self):
        for c in self.clients:
            for o in c.objs.values():
                if o.iface == 'wl_surface':
                    yield c, o

    def layer_rect(self, s):
        """Rectangle of a mapped layer surface in output coordinates."""
        L = s.layer_obj
        w, h = s.lw, s.lh
        t, r, b, l = L.margin
        a = L.anchor
        x = l if a & 4 and not a & 8 else (W - w - r if a & 8 and not a & 4 else (W - w) // 2)
        y = t if a & 1 and not a & 2 else (H - h - b if a & 2 and not a & 1 else (H - h) // 2)
        return x, y, w, h

    def hit(self, px, py):
        best = None
        for c, s in self.surfaces():
            if getattr(s, 'layer_obj', None) is None or s.buffer is None or not s.mapped:
                continue
            x, y, w, h = self.layer_rect(s)
            lx, ly = px - x, py - y
            if not (0 <= lx < w and 0 <= ly < h):
                continue
            if s.input_region is not None and not any(rx <= lx < rx + rw and ry <= ly < ry + rh for rx, ry, rw, rh in s.input_region):
                continue
            key = (s.layer_obj.layer, s.order)
            if best is None or key > best[0]:
                best = (key, c, s, lx, ly)
        return best


order_counter = [0]


# ----------------------------------------------------------------- request handling
def parse_args(c, o, name, sig, data):
    pos = 0
    out = []

    def take(n):
        nonlocal pos
        if pos + n > len(data):
            raise ProtocolError(o.id, 1, f'{o.iface}.{name}: message too short')
        v = data[pos:pos + n]
        pos += n
        return v

    def string():
        n = struct.unpack('<I', take(4))[0]
        if n == 0:
            return None
        b = take((n + 3) & ~3)
        if b[n - 1] != 0:
            raise ProtocolError(o.id, 1, f'{o.iface}.{name}: string not NUL terminated')
        return b[:n - 1].decode()

    for t in sig:
        if t in ('u', 'new_untyped') or t.startswith('obj') or t.startswith('new:'):
            if t == 'new_untyped':
                out.append(string())
                out.append(struct.unpack('<I', take(4))[0])
                out.append(struct.unpack('<I', take(4))[0])
            else:
                out.append(struct.unpack('<I', take(4))[0])
        elif t == 'i':
            out.append(struct.unpack('<i', take(4))[0])
        elif t == 'fixed':
            out.append(struct.unpack('<i', take(4))[0] / 256)
        elif t == 's':
            out.append(string())
        elif t == 'fd':
            if not c.fds:
                raise ProtocolError(o.id, 1, f'{o.iface}.{name}: expected a file descriptor')
            out.append(c.fds.pop(0))
        else:
            raise ValueError(t)
    if pos != len(data):
        raise ProtocolError(o.id, 1, f'{o.iface}.{name}: {len(data) - pos} unexpected trailing bytes')
    return out


def new_obj(c, oid, iface, version, **kw):
    if oid in c.limbo:
        raise ProtocolError(1, 0, f'new id {oid} for {iface} was reused before delete_id could have reached the client')
    if oid in c.objs or oid == 0 or oid >= 0xff000000:
        raise ProtocolError(1, 0, f'invalid new id {oid} for {iface}')
    o = Obj(iface, version, **kw)
    o.id = oid
    c.objs[oid] = o
    c.max_id = max(c.max_id, oid)
    return o


def delete(c, oid):
    """The object is gone; delete_id follows a little later, like on a busy compositor.
    Until it has been sent the client must not reuse the id."""
    import time
    c.objs.pop(oid, None)
    c.limbo[oid] = time.monotonic() + 0.03


def flush_limbo(srv):
    import time
    now = time.monotonic()
    for c in srv.clients:
        for oid in [i for i, t in c.limbo.items() if t <= now]:
            del c.limbo[oid]
            send_evt(c, 1, 'delete_id', oid)


def handle_request(srv, c, oid, op, data):
    o = c.objs.get(oid)
    if o is None:
        return  # a request for an object we already deleted: ignore like libwayland
    spec = TABLE.get((o.iface, 'req'), {}).get(op)
    if spec is None:
        raise ProtocolError(oid, 1, f'{o.iface}: no request with opcode {op}')
    name, sig = spec
    a = parse_args(c, o, name, sig, data)
    h = HANDLERS.get((o.iface, name))
    if h:
        h(srv, c, o, *a)
    elif name in ('destroy', 'release'):
        delete(c, oid)
    else:
        raise ProtocolError(oid, 1, f'{o.iface}.{name} is not implemented by the mock')


def h_sync(srv, c, o, cb):
    cbo = new_obj(c, cb, 'wl_callback', 1)
    send_evt(c, cb, 'done', 0)
    delete(c, cb)


def h_get_registry(srv, c, o, rid):
    new_obj(c, rid, 'wl_registry', 1)
    for name, iface, ver in srv.globals:
        send_evt(c, rid, 'global', name, iface, ver)


def h_bind(srv, c, o, name, iface, version, nid):
    g = [x for x in srv.globals if x[0] == name]
    if not g or g[0][1] != iface:
        raise ProtocolError(o.id, 0, f'bind of unknown global {name} {iface}')
    if not 1 <= version <= g[0][2]:
        raise ProtocolError(o.id, 0, f'{iface}: version {version} not supported (have {g[0][2]})')
    obj = new_obj(c, nid, iface, version)
    emit(ev='bind', interface=iface, version=version)
    if iface == 'wl_shm':
        for f in srv.shm_formats:
            send_evt(c, nid, 'format', f)
    elif iface == 'wl_seat':
        send_evt(c, nid, 'capabilities', 1 if srv.pointer else 0)
    elif iface == 'wl_output':
        send_evt(c, nid, 'geometry', 0, 0, 340, 190, 0, 'mock', 'mock', 0)
        send_evt(c, nid, 'mode', 3, W, H, 60000)
        send_evt(c, nid, 'scale', max(1, -(-srv.scale120 // 120)))
        send_evt(c, nid, 'done')
        obj.is_output = True


def h_create_surface(srv, c, o, nid):
    order_counter[0] += 1
    s = new_obj(c, nid, 'wl_surface', o.version, buffer=None, pending_buf=None, layer_obj=None, mapped=False,
                input_region=None, pending_input=None, viewport=None, scale=1, pending_scale=1, lw=0, lh=0,
                order=order_counter[0], frames=[], commits=0, prev_buffer=None, entered=False)
    if o.version >= 6:
        send_evt(c, nid, 'preferred_buffer_scale', max(1, -(-srv.scale120 // 120)))


def h_create_region(srv, c, o, nid):
    new_obj(c, nid, 'wl_region', 1, rects=[])


def h_region_add(srv, c, o, x, y, w, h):
    o.rects.append((x, y, w, h))


def h_create_pool(srv, c, o, nid, fd, size):
    if size <= 0:
        raise ProtocolError(o.id, 1, 'pool size must be positive')
    try:
        mm = mmap.mmap(fd, size, mmap.MAP_SHARED, mmap.PROT_READ)
    except Exception as e:
        raise ProtocolError(o.id, 1, f'cannot map the pool: {e}')
    os.close(fd)
    new_obj(c, nid, 'wl_shm_pool', 1, mm=mm, size=size)


def h_create_buffer(srv, c, o, nid, off, w, h, stride, fmt):
    if fmt not in srv.shm_formats:
        raise ProtocolError(o.id, 0, f'unsupported shm format {fmt}')
    if off < 0 or w <= 0 or h <= 0 or stride < w * 4 or off + stride * h > o.size:
        raise ProtocolError(o.id, 2, f'buffer {w}x{h} stride {stride} offset {off} does not fit the pool of {o.size} bytes')
    new_obj(c, nid, 'wl_buffer', 1, mm=o.mm, off=off, w=w, h=h, stride=stride, fmt=fmt, released=True)
    emit(ev='buffer', id=nid, size=f'{w}x{h}')


def h_pool_destroy(srv, c, o):
    delete(c, o.id)   # the mapping lives on in the buffers


def h_buffer_destroy(srv, c, o):
    delete(c, o.id)   # what is on screen stays on screen


def h_attach(srv, c, o, buf, x, y):
    if x or y:
        raise ProtocolError(o.id, 3, 'attach with a non-zero offset (use wl_surface.offset)')
    if buf == 0:
        o.pending_buf = 'null'
    else:
        b = c.objs.get(buf)
        if b is None or b.iface != 'wl_buffer':
            raise ProtocolError(o.id, 0, f'attach of {buf}, which is not a wl_buffer')
        o.pending_buf = b


def h_damage_buffer(srv, c, o, x, y, w, h):
    pass


def h_set_buffer_scale(srv, c, o, s):
    if s < 1:
        raise ProtocolError(o.id, 0, 'invalid scale')
    o.pending_scale = s


def h_set_input_region(srv, c, o, rid):
    if rid == 0:
        o.pending_input = 'all'
    else:
        r = c.objs.get(rid)
        if r is None or r.iface != 'wl_region':
            raise ProtocolError(o.id, 0, 'not a region')
        o.pending_input = list(r.rects)


def h_frame(srv, c, o, cb):
    new_obj(c, cb, 'wl_callback', 1)
    o.frames.append(cb)


def png_dump(srv, c, s, buf):
    from PIL import Image
    raw = bytes(buf.mm[buf.off:buf.off + buf.stride * buf.h])
    # wl_shm argb8888 is little endian: bytes B, G, R, A, premultiplied
    out = Image.frombuffer('RGBa', (buf.w, buf.h), raw, 'raw', 'BGRa', buf.stride, 1).convert('RGBA')
    srv.dump_n += 1
    ns = s.layer_obj.namespace if s.layer_obj else 'surface'
    path = os.path.join(srv.dumpdir, f'{ns}-{srv.dump_n:04d}.png')
    out.save(path)
    out.save(os.path.join(srv.dumpdir, f'{ns}-latest.png'))
    return path


def h_commit(srv, c, o):
    L = o.layer_obj
    # apply pending state
    if o.pending_scale != o.scale:
        o.scale = o.pending_scale
    if o.pending_input is not None:
        o.input_region = None if o.pending_input == 'all' else o.pending_input
        o.pending_input = None
    pb = o.pending_buf
    o.pending_buf = None
    if pb is not None and pb != 'null' and L is not None and not L.acked:
        raise ProtocolError(L.id, 2, 'a buffer was attached before the layer surface was configured and acked')
    if pb == 'null':
        o.buffer = None
        o.mapped = False
    elif pb is not None:
        old = o.buffer
        o.buffer = pb
        pb.released = False
        if old is not None and old is not pb and not srv.hold:
            old.released = True
            if old.id in c.objs:
                send_evt(c, old.id, 'release')
        # surface size
        vp = o.viewport
        if vp is not None and vp.dest is not None:
            o.lw, o.lh = vp.dest
        else:
            if pb.w % o.scale or pb.h % o.scale:
                raise ProtocolError(o.id, 0, f'buffer {pb.w}x{pb.h} is not a multiple of scale {o.scale}')
            o.lw, o.lh = pb.w // o.scale, pb.h // o.scale
        o.commits += 1
        if L is not None and not o.mapped:
            o.mapped = True
            for oid, ob in list(c.objs.items()):
                if getattr(ob, 'is_output', False):
                    send_evt(c, o.id, 'enter', oid)
        dump = None
        if srv.dumpdir:
            dump = png_dump(srv, c, o, pb)
        emit(ev='commit', surface=o.id, namespace=L.namespace if L else None, buffer=f'{pb.w}x{pb.h}',
             scale=o.scale, logical=f'{o.lw}x{o.lh}',
             viewport=f'{vp.dest[0]}x{vp.dest[1]}' if vp is not None and vp.dest else None, png=dump)
    # frame callbacks
    for cb in o.frames:
        send_evt(c, cb, 'done', 0)
        delete(c, cb)
    o.frames = []
    # layer surface: configure on first commit, and whenever the wanted size changed
    if L is not None:
        want = layer_wanted_size(srv, L)
        if L.last_conf is None or (pb is None and want != L.last_conf):
            first = L.last_conf is None
            L.last_conf = want
            L.conf_serial = c.serial()
            if first:
                L.acked = False   # buffers are only legal after the first ack
            send_evt(c, L.id, 'configure', L.conf_serial, want[0], want[1])
            emit(ev='configure', namespace=L.namespace, size=f'{want[0]}x{want[1]}')
            o.lw, o.lh = (o.lw or want[0]), (o.lh or want[1])


def layer_wanted_size(srv, L):
    w, h = L.size
    a = L.anchor
    if w == 0:
        if a & 4 and a & 8:
            w = W - L.margin[1] - L.margin[3]
        else:
            raise ProtocolError(L.id, 1, 'width 0 requires anchoring to both left and right')
    if h == 0:
        if a & 1 and a & 2:
            h = H - L.margin[0] - L.margin[2]
        else:
            raise ProtocolError(L.id, 1, 'height 0 requires anchoring to both top and bottom')
    return (w, h)


def h_surface_destroy(srv, c, o):
    if o.layer_obj is not None and o.layer_obj.id in c.objs:
        raise ProtocolError(o.id, 0, 'wl_surface destroyed before its layer surface role object')
    if c.focus is o:
        c.focus = None
    delete(c, o.id)


def h_get_layer_surface(srv, c, o, nid, surface, output, layer, ns):
    s = c.objs.get(surface)
    if s is None or s.iface != 'wl_surface':
        raise ProtocolError(o.id, 0, 'not a surface')
    if s.layer_obj is not None:
        raise ProtocolError(o.id, 2, 'surface already has a layer role')
    if s.buffer is not None or s.pending_buf is not None:
        raise ProtocolError(o.id, 1, 'surface already has a buffer attached')
    if layer > 3:
        raise ProtocolError(o.id, 1, f'invalid layer {layer}')
    L = new_obj(c, nid, 'zwlr_layer_surface_v1', o.version, surface=s, layer=layer, namespace=ns or '',
                size=(0, 0), anchor=0, zone=0, margin=[0, 0, 0, 0], kbd=0, acked=False, last_conf=None, conf_serial=0)
    s.layer_obj = L
    emit(ev='layer_surface', namespace=ns, layer=layer)


def h_set_size(srv, c, o, w, h):
    o.size = (w, h)


def h_set_anchor(srv, c, o, a):
    if a > 15:
        raise ProtocolError(o.id, 2, 'invalid anchor')
    o.anchor = a


def h_set_zone(srv, c, o, z):
    o.zone = z
    emit(ev='zone', namespace=o.namespace, zone=z)


def h_set_margin(srv, c, o, t, r, b, l):
    o.margin = [t, r, b, l]


def h_set_kbd(srv, c, o, k):
    o.kbd = k
    emit(ev='keyboard', namespace=o.namespace, mode=k)


def h_ack(srv, c, o, serial):
    if serial != o.conf_serial:
        # an older serial is legal (compositors may have sent several); a never-sent one is not
        if serial > o.conf_serial or serial == 0:
            raise ProtocolError(o.id, 0, f'ack_configure with unknown serial {serial}')
    o.acked = True


def h_layer_destroy(srv, c, o):
    o.surface.layer_obj = None
    o.surface.mapped = False
    delete(c, o.id)


def h_get_viewport(srv, c, o, nid, surface):
    s = c.objs[surface]
    if s.viewport is not None:
        raise ProtocolError(o.id, 0, 'viewport already exists')
    v = new_obj(c, nid, 'wp_viewport', 1, surface=s, dest=None)
    s.viewport = v


def h_set_destination(srv, c, o, w, h):
    if (w, h) == (-1, -1):
        o.dest = None
    elif w <= 0 or h <= 0:
        raise ProtocolError(o.id, 0, 'bad destination size')
    else:
        o.dest = (w, h)


def h_viewport_destroy(srv, c, o):
    o.surface.viewport = None
    delete(c, o.id)


def h_get_frac(srv, c, o, nid, surface):
    s = c.objs[surface]
    f = new_obj(c, nid, 'wp_fractional_scale_v1', 1, surface=s)
    send_evt(c, nid, 'preferred_scale', srv.scale120)


def h_get_pointer(srv, c, o, nid):
    new_obj(c, nid, 'wl_pointer', o.version)


def h_get_cursor_dev(srv, c, o, nid, ptr):
    p = c.objs.get(ptr)
    if p is None or p.iface != 'wl_pointer':
        raise ProtocolError(o.id, 0, 'not a wl_pointer')
    new_obj(c, nid, 'wp_cursor_shape_device_v1', 1, pointer=p)


def h_set_shape(srv, c, o, serial, shape):
    if not 1 <= shape <= 34:
        raise ProtocolError(o.id, 1, 'invalid shape')
    emit(ev='cursor', shape=shape, serial=serial, valid=(serial == c.enter_serial and c.focus is not None))


def h_pointer_release(srv, c, o):
    delete(c, o.id)


HANDLERS = {
    ('wl_display', 'sync'): h_sync,
    ('wl_display', 'get_registry'): h_get_registry,
    ('wl_registry', 'bind'): h_bind,
    ('wl_compositor', 'create_surface'): h_create_surface,
    ('wl_compositor', 'create_region'): h_create_region,
    ('wl_region', 'add'): h_region_add,
    ('wl_shm', 'create_pool'): h_create_pool,
    ('wl_shm_pool', 'create_buffer'): h_create_buffer,
    ('wl_shm_pool', 'destroy'): h_pool_destroy,
    ('wl_buffer', 'destroy'): h_buffer_destroy,
    ('wl_surface', 'attach'): h_attach,
    ('wl_surface', 'damage_buffer'): h_damage_buffer,
    ('wl_surface', 'set_buffer_scale'): h_set_buffer_scale,
    ('wl_surface', 'set_input_region'): h_set_input_region,
    ('wl_surface', 'frame'): h_frame,
    ('wl_surface', 'commit'): h_commit,
    ('wl_surface', 'destroy'): h_surface_destroy,
    ('zwlr_layer_shell_v1', 'get_layer_surface'): h_get_layer_surface,
    ('zwlr_layer_surface_v1', 'set_size'): h_set_size,
    ('zwlr_layer_surface_v1', 'set_anchor'): h_set_anchor,
    ('zwlr_layer_surface_v1', 'set_exclusive_zone'): h_set_zone,
    ('zwlr_layer_surface_v1', 'set_margin'): h_set_margin,
    ('zwlr_layer_surface_v1', 'set_keyboard_interactivity'): h_set_kbd,
    ('zwlr_layer_surface_v1', 'ack_configure'): h_ack,
    ('zwlr_layer_surface_v1', 'destroy'): h_layer_destroy,
    ('wp_viewporter', 'get_viewport'): h_get_viewport,
    ('wp_viewport', 'set_destination'): h_set_destination,
    ('wp_viewport', 'destroy'): h_viewport_destroy,
    ('wp_fractional_scale_manager_v1', 'get_fractional_scale'): h_get_frac,
    ('wl_seat', 'get_pointer'): h_get_pointer,
    ('wp_cursor_shape_manager_v1', 'get_pointer'): h_get_cursor_dev,
    ('wp_cursor_shape_device_v1', 'set_shape'): h_set_shape,
    ('wl_pointer', 'release'): h_pointer_release,
}

# ----------------------------------------------------------------- commands (stdin)
def by_ns(srv, ns):
    for c, s in srv.surfaces():
        if s.layer_obj is not None and s.layer_obj.namespace == ns:
            return c, s
    return None, None


def pointers(c):
    return [o for o in c.objs.values() if o.iface == 'wl_pointer']


def move_pointer(srv, px, py):
    h = srv.hit(px, py)
    for c in srv.clients:
        ps = pointers(c)
        new = h[2] if h and h[1] is c else None
        if c.focus is not None and c.focus is not new:
            for p in ps:
                send_evt(c, p.id, 'leave', c.serial(), c.focus.id)
                if p.version >= 5:
                    send_evt(c, p.id, 'frame')
            c.focus = None
        if new is not None:
            lx, ly = h[3], h[4]
            for p in ps:
                if c.focus is None:
                    c.enter_serial = c.serial()
                    send_evt(c, p.id, 'enter', c.enter_serial, new.id, lx, ly)
                send_evt(c, p.id, 'motion', int(time_ms()), lx, ly)
                if p.version >= 5:
                    send_evt(c, p.id, 'frame')
            c.focus = new
            c.last_local = (lx, ly)


def time_ms():
    import time
    return time.monotonic() * 1000 % 0xffffffff


def command(srv, line):
    f = line.split()
    if not f:
        return True
    cmd = f[0]
    if cmd == 'quit':
        return False
    elif cmd == 'scale':
        srv.scale120 = int(f[1])
        for c in srv.clients:
            for oid, ob in list(c.objs.items()):
                if getattr(ob, 'is_output', False):
                    send_evt(c, oid, 'scale', max(1, -(-srv.scale120 // 120)))
                    send_evt(c, oid, 'done')
                if ob.iface == 'wp_fractional_scale_v1':
                    send_evt(c, oid, 'preferred_scale', srv.scale120)
                if ob.iface == 'wl_surface' and ob.version >= 6:
                    send_evt(c, oid, 'preferred_buffer_scale', max(1, -(-srv.scale120 // 120)))
    elif cmd == 'move':
        move_pointer(srv, float(f[1]), float(f[2]))
    elif cmd in ('press', 'release_button'):
        code = int(f[1]) if len(f) > 1 else 0x110
        for c in srv.clients:
            if c.focus is not None:
                for p in pointers(c):
                    send_evt(c, p.id, 'button', c.serial(), int(time_ms()), code, 1 if cmd == 'press' else 0)
                    if p.version >= 5:
                        send_evt(c, p.id, 'frame')
    elif cmd == 'click':
        move_pointer(srv, float(f[1]), float(f[2]))
        command(srv, 'press')
        command(srv, 'release_button')
    elif cmd == 'axis':
        for c in srv.clients:
            if c.focus is not None:
                for p in pointers(c):
                    send_evt(c, p.id, 'axis', int(time_ms()), 0, float(f[1]))
                    send_evt(c, p.id, 'frame')
    elif cmd == 'leave':
        move_pointer(srv, -1, -1)
    elif cmd == 'configure':
        c, s = by_ns(srv, f[1])
        if s is not None:
            L = s.layer_obj
            L.conf_serial = c.serial()
            L.acked = False
            L.last_conf = (int(f[2]), int(f[3]))
            send_evt(c, L.id, 'configure', L.conf_serial, int(f[2]), int(f[3]))
    elif cmd == 'close':
        c, s = by_ns(srv, f[1])
        if s is not None:
            send_evt(c, s.layer_obj.id, 'closed')
    elif cmd == 'release':
        for c, s in srv.surfaces():
            for o in list(c.objs.values()):
                if o.iface == 'wl_buffer' and not o.released:
                    o.released = True
                    send_evt(c, o.id, 'release')
    elif cmd == 'hold':
        srv.hold = f[1] == 'on'
    elif cmd == 'error':
        for c in srv.clients:
            send_evt(c, 1, 'error', 1, 7, 'forced error')
    elif cmd == 'disconnect':
        for c in srv.clients:
            c.dead = True
    elif cmd == 'state':
        for c, s in srv.surfaces():
            L = s.layer_obj
            emit(ev='surface', namespace=L.namespace if L else None, mapped=s.mapped, logical=[s.lw, s.lh],
                 rect=list(srv.layer_rect(s)) if L and s.lw else None, anchor=L.anchor if L else None,
                 margin=L.margin if L else None, zone=L.zone if L else None, layer=L.layer if L else None,
                 input=[list(r) for r in s.input_region][0] if s.input_region else None)
        emit(ev='state_end', objects=sum(len(c.objs) for c in srv.clients), max_id=max([c.max_id for c in srv.clients] or [0]))
    else:
        emit(ev='bad_command', line=line)
    return True


# ----------------------------------------------------------------- main loop
def main():
    args = sys.argv[1:]
    load_table(args.pop(0))
    without, pointer, dumpdir = set(), True, None
    while args:
        a = args.pop(0)
        if a == '--without':
            without = set(args.pop(0).split(','))
        elif a == '--no-pointer':
            pointer = False
        elif a == '--dump':
            dumpdir = args.pop(0)
            os.makedirs(dumpdir, exist_ok=True)
    srv = Server(without, pointer, dumpdir)
    path = os.path.join(os.environ['XDG_RUNTIME_DIR'], os.environ.get('WAYLAND_DISPLAY', 'wayland-0'))
    if os.path.exists(path):
        os.unlink(path)
    ls = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    ls.bind(path)
    ls.listen(4)
    emit(ev='ready', socket=path)
    stdin_buf = b''
    running = True
    while running:
        rl = [ls, sys.stdin] + [c.sock for c in srv.clients if not c.dead]
        r, _, _ = select.select(rl, [], [], 0.01 if any(c.limbo for c in srv.clients) else 0.5)
        flush_limbo(srv)
        for x in r:
            if x is ls:
                s, _ = ls.accept()
                srv.clients.append(Client(s))
                emit(ev='client')
            elif x is sys.stdin:
                d = os.read(0, 4096)
                if not d:
                    running = False
                    break
                stdin_buf += d
                while b'\n' in stdin_buf:
                    line, stdin_buf = stdin_buf.split(b'\n', 1)
                    if not command(srv, line.decode()):
                        running = False
            else:
                c = next(c for c in srv.clients if c.sock is x)
                try:
                    data, anc, flags, _ = c.sock.recvmsg(65536, socket.CMSG_SPACE(16 * 4))
                except OSError:
                    c.dead = True
                    continue
                for level, typ, cd in anc:
                    if level == socket.SOL_SOCKET and typ == socket.SCM_RIGHTS:
                        a = array.array('i')
                        a.frombytes(cd[:len(cd) - (len(cd) % 4)])
                        c.fds.extend(a)
                if not data:
                    c.dead = True
                    continue
                c.buf += data
                try:
                    while len(c.buf) >= 8 and not c.dead:
                        oid, w = struct.unpack('<II', c.buf[:8])
                        size, op = w >> 16, w & 0xffff
                        if size < 8 or size % 4:
                            raise ProtocolError(oid, 1, f'bad message size {size}')
                        if len(c.buf) < size:
                            break
                        body = c.buf[8:size]
                        c.buf = c.buf[size:]
                        handle_request(srv, c, oid, op, body)
                except ProtocolError as e:
                    fail(c, e.obj, e.code, e.msg)
        for c in [c for c in srv.clients if c.dead]:
            try:
                c.sock.close()
            except OSError:
                pass
            srv.clients.remove(c)
            emit(ev='client_gone')
    try:
        os.unlink(path)
    except OSError:
        pass


if __name__ == '__main__':
    main()
