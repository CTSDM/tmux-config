"""record.py's cast -> frames (agg) -> MP4 with a caption band (ffmpeg) ->
the README's GIF. Needs agg (github.com/asciinema/agg), ffmpeg, the
JetBrainsMono Nerd Font and Noto Sans CJK.

    python3 tests/demo/render.py /tmp/demo.cast docs/demo.gif
"""

import json
import subprocess
import sys
from pathlib import Path

cast = Path(sys.argv[1])
d = cast.parent
gif = Path(sys.argv[2])
# Captions: a font with ● ✓ ○ ▲ ◐ and the dakuten.
font = subprocess.run(
    ["fc-match", "-f", "%{file}", "Noto Sans CJK JP:style=Medium"],
    capture_output=True,
    text=True,
    check=True,
).stdout
theme = "1e1e2e,cdd6f4,45475a,f38ba8,a6e3a1,f9e2af,89b4fa,cba6f7,94e2d5,bac2de,585b70,f38ba8,a6e3a1,f9e2af,89b4fa,cba6f7,94e2d5,a6adc8"
base = d / "base.gif"
subprocess.run(
    [
        "agg",
        "-q",
        "--font-family",
        "JetBrainsMono Nerd Font,DejaVu Sans Mono,Noto Sans CJK JP",
        "--font-size",
        "18",
        "--theme",
        theme,
        "--idle-time-limit",
        "30",
        "--last-frame-duration",
        "2",
        str(cast),
        str(base),
    ],
    check=True,
)
caps = json.loads(cast.with_suffix(".captions.json").read_text())
filters = ["pad=iw:ih+56:0:0:color=0x181825"]
for i, (a, b, text) in enumerate(caps):
    f = d / f"cap{i}.txt"
    f.write_text(text)
    filters.append(
        f"drawtext=fontfile='{font}':textfile='{f}':fontsize=24:fontcolor=0xcdd6f4:"
        f"x=(w-text_w)/2:y=h-40:enable='between(t,{a},{b})'"
    )
vf = ",".join(filters) + ",scale=trunc(iw/2)*2:trunc(ih/2)*2"
mp4 = d / "demo.mp4"
subprocess.run(
    [
        "ffmpeg",
        "-loglevel",
        "error",
        "-y",
        "-i",
        str(base),
        "-vf",
        vf,
        "-r",
        "30",
        "-pix_fmt",
        "yuv420p",
        "-c:v",
        "libx264",
        "-crf",
        "20",
        str(mp4),
    ],
    check=True,
)
subprocess.run(
    [
        "ffmpeg",
        "-loglevel",
        "error",
        "-y",
        "-i",
        str(mp4),
        "-vf",
        "fps=12,scale=1100:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=96[p];[b][p]paletteuse=dither=none",
        str(gif),
    ],
    check=True,
)
for p in (mp4, gif):
    print(p, p.stat().st_size // 1024, "KB")
