# Third-party notices

CineFS itself is licensed under MIT or Apache-2.0 (see [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE)). The following third-party components are distributed with it.

## WinFsp (Windows builds only)

WinFsp - Windows File System Proxy, Copyright (C) Bill Zissimopoulos
<https://github.com/winfsp/winfsp>

The Windows installer (`CineFS-Setup.exe`) and the portable `CineFS.exe` include the unmodified
WinFsp 2.0 runtime installer (`winfsp-2.0.23075.msi`), exactly as released by the WinFsp project,
and install it when it is missing. CineFS loads the WinFsp DLL at runtime.

WinFsp is licensed under the GPLv3 with a special exception for Free/Libre and Open Source
Software, which permits FLOSS projects such as CineFS to link with the WinFsp DLL and to
redistribute the unmodified WinFsp installer. See the
[WinFsp license](https://github.com/winfsp/winfsp/blob/master/License.txt).

## Geist and Geist Mono fonts (website only)

Copyright 2026 The Geist Project Authors (<https://github.com/vercel/geist-font>)

The project website (`docs/`) uses the Geist and Geist Mono fonts, licensed under the SIL Open Font
License, Version 1.1. The full license text is in [docs/fonts/OFL.txt](docs/fonts/OFL.txt). The fonts
are not included in the CineFS applications.

## Magic Lantern focus pixel maps

The bundled focus-pixel coordinate maps (`crates/cinefs-core/data/focus_maps.bin`, covering Canon
100D, 650D, 700D, EOS M and EOS M2 readout modes) are derived from the community-measured focus
pixel maps published by the Magic Lantern project (<https://www.magiclantern.fm>).
