"""Extract EXIF-upright full-size JPEGs from NEFs into <out>/img/p<N>.jpg.

python extract.py <out_dir> <nef> [<nef> ...]   (needs exiftool on PATH; Pillow)
"""
import io, subprocess, sys, os
from PIL import Image

out = sys.argv[1]
os.makedirs(out + "/img", exist_ok=True)
for n, nef in enumerate(sys.argv[2:], 1):
    raw = subprocess.run(["exiftool", "-b", "-JpgFromRaw", "--", nef], capture_output=True, check=True).stdout
    tag = subprocess.run(["exiftool", "-n", "-s3", "-IFD0:Orientation", "--", nef], capture_output=True, text=True, check=True).stdout.strip()
    if not tag:
        sys.exit(f"{nef}: no EXIF Orientation; refusing to guess (sideways inputs skew the results)")
    o = int(tag)
    im = Image.open(io.BytesIO(raw)).convert("RGB")
    # EXIF orientation -> transpose op (the embedded JPEG itself is unrotated)
    ops = {2: Image.FLIP_LEFT_RIGHT, 3: Image.ROTATE_180, 4: Image.FLIP_TOP_BOTTOM, 5: Image.TRANSPOSE,
           6: Image.ROTATE_270, 7: Image.TRANSVERSE, 8: Image.ROTATE_90}
    if o in ops:
        im = im.transpose(ops[o])
    im.save(f"{out}/img/p{n}.jpg", quality=95)
