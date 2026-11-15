"""Populates mounted NTFS test images and records ground truth.

Driven by scripts/make-ntfs-fixtures.sh (run it, not this file). Commands:

  populate <mountpoint> <profile> <cluster_size> <record_size> <manifest.json>
      Fill a freshly formatted image mounted with ntfs-3g
      (-o streams_interface=windows,compression). Profiles: rich, many, big.
      Writes a manifest of the special objects it created.
  truth <mountpoint> <manifest.json> <out.json>
      Walk the (read-only, freshly mounted) image with os.lstat and write the
      ground truth: one entry per path with is_dir, symlink, size,
      st_blocks*512, nlink and inode (= MFT record number under ntfs-3g),
      plus stat() of every alternate data stream from the manifest.
  facts <image> <out.json> <key> <ntfs path>
      Count runs / detect $ATTRIBUTE_LIST with ntfsinfo for one file (or
      "$MFT" for record 0) and store them under facts.<key> in out.json.

Everything is deterministic (fixed seeds) so images are reproducible.
Run with `python3 -I`.
"""

import errno
import json
import os
import random
import stat
import struct
import subprocess
import sys

# FILE_ATTRIBUTE_* used through ntfs-3g's system.ntfs_attrib_be xattr.
ATTR_COMPRESSED = 0x800
ATTR_DIRECTORY = 0x10

TAG_MOUNT_POINT = 0xA0000003
TAG_SYMLINK = 0xA000000C
TAG_CLOUD_6 = 0x9000601A


def write(path, data):
    with open(path, 'wb') as f:
        f.write(data)


def mkdirs(path):
    os.makedirs(path, exist_ok=True)
    return path


def reparse_junction(target):
    sub = ('\\??\\' + target).encode('utf-16-le')
    pr = target.encode('utf-16-le')
    body = struct.pack('<HHHH', 0, len(sub), len(sub) + 2, len(pr)) + sub + b'\0\0' + pr + b'\0\0'
    return struct.pack('<IHH', TAG_MOUNT_POINT, len(body), 0) + body


def reparse_symlink(target):
    sub = ('\\??\\' + target).encode('utf-16-le')
    pr = target.encode('utf-16-le')
    body = struct.pack('<HHHHI', 0, len(sub), len(sub), len(pr), 0) + sub + pr
    return struct.pack('<IHH', TAG_SYMLINK, len(body), 0) + body


def reparse_generic(tag, payload):
    # Microsoft tags (bit 31) carry no GUID.
    return struct.pack('<IHH', tag, len(payload), 0) + payload


class Manifest:
    def __init__(self, root):
        self.root = root
        self.data = {
            'streams': [],      # [path, stream name]
            'reparse': {},      # path -> tag
            'dos_names': {},    # path -> short name
            'compressed': [],
            'sparse': [],
            'deleted': [],      # paths that must not exist
            'fragmented': [],   # paths of deliberately fragmented files
            'interix_symlinks': [],
            'notes': [],
        }

    def rel(self, path):
        return os.path.relpath(path, self.root).replace(os.sep, '/')

    def save(self, out):
        with open(out, 'w', encoding='utf-8') as f:
            json.dump(self.data, f, ensure_ascii=False, indent=1)


def sync_write(path, data, mode='wb'):
    with open(path, mode) as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())


def fill(path, limit):
    """Writes up to `limit` bytes (None = until the volume is full)."""
    chunk = b'F' * (1 << 20)
    done = 0
    try:
        with open(path, 'wb') as f:
            while limit is None or done < limit:
                n = len(chunk) if limit is None else min(len(chunk), limit - done)
                f.write(chunk[:n])
                done += n
            f.flush()
            os.fsync(f.fileno())
    except OSError as e:
        if e.errno != errno.ENOSPC:
            raise
    return done


def fragmented_file(m, root, cluster, record, appends):
    """Builds frag/fragA.bin with about `appends` fragments: fills the volume
    so that the only free space is a comb of small holes (every other "tooth"
    file deleted), then grows the file one cluster at a time, so every append
    lands in a different hole. ntfs-3g allocates forward from its current
    zone position, so holes elsewhere are the only way to force fragments.
    With enough fragments the runlist no longer fits the record and ntfs-3g
    moves it to extension records listed in an $ATTRIBUTE_LIST."""
    d = mkdirs(os.path.join(root, 'frag'))
    comb = mkdirs(os.path.join(d, 'comb'))
    target = os.path.join(d, 'fragA.bin')
    tooth = record - 128  # smallest size that is never resident
    tooth_clusters = -(-tooth // cluster)
    holes = -(-appends // tooth_clusters) + 8
    sync_write(target, b'A' * max(cluster, tooth))
    st = os.statvfs(root)
    free = st.f_bavail * st.f_frsize
    reserve = 2 * holes * (tooth_clusters * cluster + record) + (4 << 20)
    fillers = [os.path.join(d, 'filler1.bin'), os.path.join(d, 'filler2.bin')]
    fill(fillers[0], max(0, free - reserve))
    teeth = 0
    try:
        for i in range(2 * holes):
            sync_write(os.path.join(comb, f't{i:05d}'), b't' * tooth)
            teeth += 1
    except OSError as e:
        if e.errno != errno.ENOSPC:
            raise
    fill(fillers[1], None)
    for i in range(0, teeth, 2):
        p = os.path.join(comb, f't{i:05d}')
        os.unlink(p)
        m.data['deleted'].append(m.rel(p))
    try:
        for i in range(appends):
            sync_write(target, bytes([i % 251]) * cluster, 'ab')
    except OSError as e:
        if e.errno != errno.ENOSPC:
            raise
    for p in fillers:
        os.unlink(p)
        m.data['deleted'].append(m.rel(p))
    m.data['fragmented'].append(m.rel(target))


def populate_rich(root, cluster, record, m):
    rnd = random.Random(1234)

    # Nested directories, depth 14, a file at every level.
    p = root
    for i in range(14):
        p = mkdirs(os.path.join(p, f'level{i:02d}'))
        write(os.path.join(p, f'file{i:02d}.txt'), b'n' * (i * 311))
    write(os.path.join(p, 'deepest.bin'), bytes(rnd.getrandbits(8) for _ in range(9000)))

    # One directory with 5000 entries (multi-block $I30 index), mixed sizes.
    big = mkdirs(os.path.join(root, 'bigdir'))
    for i in range(5000):
        kind = i % 3
        size = 0 if kind == 0 else (40 + i % 500 if kind == 1 else 1200 + (i * 37) % 5000)
        write(os.path.join(big, f'entry{i:05d}.dat'), b'b' * size)
    for i in range(5):
        mkdirs(os.path.join(big, f'subdir{i}'))

    # Hard links: 4 names in 3 directories, and 2 names in one directory.
    links = mkdirs(os.path.join(root, 'links'))
    sub = mkdirs(os.path.join(links, 'sub'))
    orig = os.path.join(links, 'orig.bin')
    write(orig, b'L' * 20000)
    os.link(orig, os.path.join(links, 'second-name.bin'))
    os.link(orig, os.path.join(sub, 'third-name.bin'))
    os.link(orig, os.path.join(root, 'level00', 'fourth-name.bin'))
    pair = os.path.join(links, 'pair-a.txt')
    write(pair, b'small resident file with two names')
    os.link(pair, os.path.join(links, 'pair-b.txt'))
    # Interix symlink (what ntfs-3g creates for symlink(2)).
    os.symlink('orig.bin', os.path.join(links, 'symlink-to-orig'))
    m.data['interix_symlinks'].append('links/symlink-to-orig')

    # Sparse files.
    sp = mkdirs(os.path.join(root, 'sparse'))
    hole = os.path.join(sp, 'hole.bin')
    with open(hole, 'wb') as f:
        f.truncate(64 << 20)
        f.seek(10 << 20)
        f.write(b'D' * 65536)
        f.seek(40 << 20)
        f.write(b'E' * 4096)
    with open(os.path.join(sp, 'all-hole.bin'), 'wb') as f:
        f.truncate(1 << 30)
    m.data['sparse'] += ['sparse/hole.bin', 'sparse/all-hole.bin']

    # Compressed files (NTFS compression needs clusters <= 4 KiB).
    comp = mkdirs(os.path.join(root, 'compressed'))
    if cluster <= 4096:
        os.setxattr(comp, 'system.ntfs_attrib_be', (ATTR_COMPRESSED | ATTR_DIRECTORY).to_bytes(4, 'big'))
        write(os.path.join(comp, 'text.txt'), b'compressible line of text 0123456789\n' * 30000)
        write(os.path.join(comp, 'random.bin'), bytes(rnd.getrandbits(8) for _ in range(200000)))
        write(os.path.join(comp, 'small.txt'), b'tiny')
        with open(os.path.join(comp, 'sparse-compressed.bin'), 'wb') as f:
            f.truncate(8 << 20)
            f.seek(1 << 20)
            f.write(b'Z' * 300000)
        m.data['compressed'] += ['compressed/text.txt', 'compressed/random.bin', 'compressed/sparse-compressed.bin']
    else:
        m.data['notes'].append('no compression: cluster > 4096')
        write(os.path.join(comp, 'text.txt'), b'not compressed\n' * 1000)

    # Alternate data streams (streams_interface=windows: "name:stream").
    ads = mkdirs(os.path.join(root, 'ads'))
    host = os.path.join(ads, 'host.txt')
    write(host, b'host file body')
    write(host + ':small', b'resident stream data')
    write(host + ':big', b'S' * 50000)
    write(host + ':empty', b'')
    m.data['streams'] += [['ads/host.txt', 'small'], ['ads/host.txt', 'big'], ['ads/host.txt', 'empty']]
    big_host = os.path.join(ads, 'big-host.bin')
    write(big_host, b'H' * 30000)
    write(big_host + ':Zone.Identifier', b'[ZoneTransfer]\r\nZoneId=3\r\n')
    m.data['streams'].append(['ads/big-host.bin', 'Zone.Identifier'])
    dir_with_stream = mkdirs(os.path.join(ads, 'dir-with-stream'))
    write(dir_with_stream + ':meta', b'M' * 6000)
    m.data['streams'].append(['ads/dir-with-stream', 'meta'])

    # Unicode names: Cyrillic, emoji (surrogate pairs), CJK, Greek,
    # combining marks, and 255-UTF-16-unit names.
    uni = mkdirs(os.path.join(root, 'unicode'))
    names = [
        'Привіт світ.txt',
        '😀🎉 файл.bin',
        '日本語のファイル.txt',
        'Ελληνικά αρχεία',
        'cafe\u0301 decomposed.txt',
        'Ї' * 255,
        'a' * 255,
        '😀' * 127 + 'z',
        'mixed ąčęėįšųūž ÄÖÜß.dat',
    ]
    for i, n in enumerate(names):
        write(os.path.join(uni, n), b'u' * (i * 100))
    mkdirs(os.path.join(uni, 'Каталог з пробілами'))
    write(os.path.join(uni, 'Каталог з пробілами', 'вкладений файл.txt'), b'nested')

    # Deleted files and a deleted directory tree.
    dl = mkdirs(os.path.join(root, 'deleted'))
    for i in range(300):
        write(os.path.join(dl, f'tmp{i:03d}.bin'), b'x' * (i * 50))
    for i in range(0, 300, 3):
        p = os.path.join(dl, f'tmp{i:03d}.bin')
        os.unlink(p)
        m.data['deleted'].append(m.rel(p))
    gone = mkdirs(os.path.join(dl, 'gone-dir'))
    for i in range(20):
        write(os.path.join(gone, f'g{i}'), b'g' * 5000)
    for i in range(20):
        os.unlink(os.path.join(gone, f'g{i}'))
    os.rmdir(gone)
    m.data['deleted'].append('deleted/gone-dir')

    # Reparse points via ntfs-3g's system.ntfs_reparse_data xattr.
    rp = mkdirs(os.path.join(root, 'reparse'))
    junction = mkdirs(os.path.join(rp, 'junction'))
    os.setxattr(junction, 'system.ntfs_reparse_data', reparse_junction('C:\\Windows'))
    cloud = os.path.join(rp, 'cloud-placeholder.txt')
    write(cloud, b'')
    os.setxattr(cloud, 'system.ntfs_reparse_data', reparse_generic(TAG_CLOUD_6, b'\x01\x00\x00\x00' + b'p' * 60))
    winlink = os.path.join(rp, 'win-symlink.txt')
    write(winlink, b'')
    os.setxattr(winlink, 'system.ntfs_reparse_data', reparse_symlink('C:\\Windows\\notepad.exe'))
    m.data['reparse'].update({
        'reparse/junction': TAG_MOUNT_POINT,
        'reparse/cloud-placeholder.txt': TAG_CLOUD_6,
        'reparse/win-symlink.txt': TAG_SYMLINK,
    })

    # DOS (8.3) names: the long name becomes Win32, plus a DOS-only name.
    dos = mkdirs(os.path.join(root, 'dosnames'))
    lf = os.path.join(dos, 'LongFileName.txt')
    write(lf, b'has a short name')
    os.setxattr(lf, 'system.ntfs_dos_name', b'LONGFI~1.TXT')
    ld = mkdirs(os.path.join(dos, 'Long Directory Name'))
    os.setxattr(ld, 'system.ntfs_dos_name', b'LONGDI~1')
    write(os.path.join(ld, 'inside.txt'), b'i')
    m.data['dos_names'].update({'dosnames/LongFileName.txt': 'LONGFI~1.TXT', 'dosnames/Long Directory Name': 'LONGDI~1'})

    # Edge sizes.
    misc = mkdirs(os.path.join(root, 'misc'))
    mkdirs(os.path.join(misc, 'empty-dir'))
    write(os.path.join(misc, 'empty.txt'), b'')
    for n in (1, 600, 700, 800, 1023, 1024, 4095, 4096, 4097, cluster, cluster + 1, 3 << 20):
        write(os.path.join(misc, f'size-{n}.bin'), b'q' * n)

    # Heavily fragmented file: >= 300 fragments and an $ATTRIBUTE_LIST
    # (a 4 KiB record holds ~800 runs, a 1 KiB record ~150). Last step: it
    # fills the volume temporarily.
    fragmented_file(m, root, cluster, record, 1200)


def populate_many(root, cluster, record, m):
    """>= 30 000 files and a fragmented $MFT with an $ATTRIBUTE_LIST:
    fill the volume with small files (data spills into the MFT zone), delete
    every other one (scattered holes), then create 40 000 more files so the
    MFT grows into the holes."""
    rnd = random.Random(99)
    fill = mkdirs(os.path.join(root, 'fill'))
    n = 0
    try:
        while True:
            if n % 1000 == 0:
                mkdirs(os.path.join(fill, f'd{n // 1000:02d}'))
            write(os.path.join(fill, f'd{n // 1000:02d}', f'f{n:05d}'), b'z' * max(2 * cluster, 8192))
            n += 1
    except OSError:
        pass  # ENOSPC: the volume is full
    m.data['notes'].append(f'filled with {n} files')
    for i in range(0, n, 2):
        p = os.path.join(fill, f'd{i // 1000:02d}', f'f{i:05d}')
        try:
            os.unlink(p)
            m.data['deleted'].append(m.rel(p))
        except FileNotFoundError:
            pass  # the file that hit ENOSPC may be missing
    many = mkdirs(os.path.join(root, 'many'))
    # First directory gets 5000 entries, the rest 2000.
    created = 0
    d = None
    for i in range(40000):
        if i == 0 or (i >= 5000 and (i - 5000) % 2000 == 0):
            d = mkdirs(os.path.join(many, f'd{created:03d}'))
            created += 1
        size = 0 if i % 4 else rnd.randrange(0, 600)
        write(os.path.join(d, f'e{i:05d}'), b'e' * size)
    # A few hard links and a sparse file for good measure.
    first = os.path.join(many, 'd000', 'e00000')
    os.link(first, os.path.join(root, 'link-to-e00000'))
    with open(os.path.join(root, 'sparse.bin'), 'wb') as f:
        f.truncate(16 << 20)


def populate_big(root, cluster, record, m):
    """~2 GB volume: a wide tree of 150 000 small files, a few large files
    far into the volume (LCNs need 3-byte runlist deltas) and a big sparse
    file."""
    rnd = random.Random(7)
    count = 0
    for a in range(30):
        da = mkdirs(os.path.join(root, f'tree{a:02d}'))
        for b in range(25):
            db = mkdirs(os.path.join(da, f'branch{b:02d}'))
            for c in range(200):
                size = rnd.choice((0, 10, 100, 500, 900, 3000, 20000)) if c % 10 == 0 else rnd.randrange(0, 400)
                write(os.path.join(db, f'leaf{c:03d}.dat'), b'l' * size)
                count += 1
    m.data['notes'].append(f'{count} leaf files')
    large = mkdirs(os.path.join(root, 'large'))
    chunk = b'\xAB' * (1 << 20)
    for i in range(3):
        with open(os.path.join(large, f'large{i}.bin'), 'wb') as f:
            for _ in range(100):
                f.write(chunk)
    with open(os.path.join(large, 'huge-sparse.bin'), 'wb') as f:
        f.truncate(1500 << 20)
        f.seek(700 << 20)
        f.write(chunk)
    m.data['sparse'].append('large/huge-sparse.bin')


def populate(mnt, profile, cluster, record, manifest_path):
    m = Manifest(mnt)
    {'rich': populate_rich, 'many': populate_many, 'big': populate_big}[profile](mnt, cluster, record, m)
    m.save(manifest_path)


def entry(rel, st):
    return {
        'path': rel,
        'dir': stat.S_ISDIR(st.st_mode),
        'symlink': stat.S_ISLNK(st.st_mode),
        'size': st.st_size,
        'alloc': st.st_blocks * 512,
        'nlink': st.st_nlink,
        'ino': st.st_ino,
    }


def truth(mnt, manifest_path, out):
    with open(manifest_path, encoding='utf-8') as f:
        manifest = json.load(f)
    files = [entry('', os.lstat(mnt))]
    stack = ['']
    while stack:
        rel = stack.pop()
        base = os.path.join(mnt, rel) if rel else mnt
        for name in sorted(os.listdir(base)):
            r = f'{rel}/{name}' if rel else name
            st = os.lstat(os.path.join(mnt, r))
            files.append(entry(r, st))
            if stat.S_ISDIR(st.st_mode):
                stack.append(r)
    streams = []
    for path, name in manifest['streams']:
        st = os.stat(os.path.join(mnt, path) + ':' + name)
        streams.append({'path': path, 'name': name, 'size': st.st_size, 'alloc': st.st_blocks * 512})
    doc = {'files': files, 'streams': streams, 'manifest': manifest, 'facts': {}}
    with open(out, 'w', encoding='utf-8') as f:
        json.dump(doc, f, ensure_ascii=False)
    print(f'  truth: {len(files)} paths, {len(streams)} streams')


def facts(image, out, key, path):
    """Runs ntfsinfo on one file and records its fragment count and whether
    it has an $ATTRIBUTE_LIST."""
    args = ['ntfsinfo', '-v']
    args += ['-i', '0'] if path == '$MFT' else ['-F', path]
    text = subprocess.run(args + [image], capture_output=True, text=True, check=True).stdout
    runs = 0
    in_data = False
    for line in text.splitlines():
        if line.startswith('Dumping attribute'):
            in_data = line.startswith('Dumping attribute $DATA')
        elif in_data:
            parts = line.split()
            if len(parts) == 3 and parts[0].startswith('0x') and parts[2].startswith('0x'):
                runs += 1
    has_list = 'Dumping attribute $ATTRIBUTE_LIST' in text
    with open(out, encoding='utf-8') as f:
        doc = json.load(f)
    doc['facts'][key] = {'path': path, 'runs': runs, 'attribute_list': has_list}
    with open(out, 'w', encoding='utf-8') as f:
        json.dump(doc, f, ensure_ascii=False)
    print(f'  {key}: {path}: {runs} runs, attribute list: {has_list}')


def main(argv):
    cmd = argv[1] if len(argv) > 1 else ''
    if cmd == 'populate' and len(argv) == 7:
        populate(argv[2], argv[3], int(argv[4]), int(argv[5]), argv[6])
    elif cmd == 'truth' and len(argv) == 5:
        truth(argv[2], argv[3], argv[4])
    elif cmd == 'facts' and len(argv) == 6:
        facts(argv[2], argv[3], argv[4], argv[5])
    else:
        print(__doc__)
        return 2
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv))
