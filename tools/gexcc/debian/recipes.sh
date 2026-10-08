# Recipes for tools/gexcc/debian/build.sh. build_SRCPKG (dashes, dots and
# pluses as underscores) runs in the unpacked Debian source tree with `set -e`
# and installs into $DEST (that package's staging tree, a merged-/usr root).
# Libraries also install into $SYSROOT so later packages link them. Options
# follow debian/rules where they mean something here; what we lack (selinux,
# audit, systemd, nls) is turned off.

# autotools in cross mode: configure runs nothing it builds (config.site holds
# what the run-time checks found on gex itself)
at_configure() {
  CONFIG_SITE=$GEXCC_PREFIX/share/gexcc/config.site ./configure --host=$T --build=x86_64-pc-linux-gnu \
    --prefix=/usr --sysconfdir=/etc --localstatedir=/var --mandir=/usr/share/man --infodir=/usr/share/info \
    --disable-dependency-tracking "$@"
}
# install into the staging tree and (for a library) the sysroot
both() { make "$@" DESTDIR=$DEST; make "$@" DESTDIR=$SYSROOT; }
# libtool archives point at /usr/lib: drop them so the driver finds .a/.so archives
nola() { find "$DEST" "$SYSROOT/usr/lib" -name '*.la' -delete 2>/dev/null || true; }

build_zlib() {
  CC=gexcc AR=gexcc-ar RANLIB=gexcc-ranlib CHOST=$T ./configure --prefix=/usr
  make -j$JOBS
  both install
}

build_bzip2() {
  make -j$JOBS CC=gexcc AR=gexcc-ar RANLIB=gexcc-ranlib libbz2.a bzip2 bzip2recover
  for d in $DEST $SYSROOT; do
    mkdir -p $d/usr/bin $d/usr/lib $d/usr/include
    cp libbz2.a $d/usr/lib/; cp bzlib.h $d/usr/include/
  done
  cp bzip2 bzip2recover bzgrep bzmore bzdiff $DEST/usr/bin/
  ln -sf bzip2 $DEST/usr/bin/bunzip2; ln -sf bzip2 $DEST/usr/bin/bzcat
  ln -sf bzgrep $DEST/usr/bin/bzegrep; ln -sf bzgrep $DEST/usr/bin/bzfgrep
  ln -sf bzmore $DEST/usr/bin/bzless; ln -sf bzdiff $DEST/usr/bin/bzcmp
}

build_xz_utils() {
  at_configure --disable-nls --disable-doc --enable-static --disable-shared
  make -j$JOBS
  both install; nola
}

build_libzstd() {
  # Debian builds lib/ and programs/ with plain make
  make -j$JOBS -C lib CC=gexcc AR=gexcc-ar libzstd.a
  make -j$JOBS -C programs CC=gexcc AR=gexcc-ar HAVE_ZLIB=0 HAVE_LZMA=0 HAVE_LZ4=0 zstd
  for d in $DEST $SYSROOT; do
    make -C lib install-static install-includes install-pc DESTDIR=$d PREFIX=/usr LIBDIR=/usr/lib
  done
  mkdir -p $DEST/usr/bin
  cp programs/zstd $DEST/usr/bin/
  for n in unzstd zstdcat zstdmt; do ln -sf zstd $DEST/usr/bin/$n; done
}

build_libmd() {
  at_configure --enable-static --disable-shared
  make -j$JOBS
  both install; nola
}

build_ncurses() {
  # Debian: wide-character libraries, a separate libtinfo, terminfo from
  # ncurses-base/ncurses-term (Architecture: all), so no database here
  at_configure --without-cxx-binding --without-ada --without-tests --enable-widec \
    --with-termlib --disable-stripping --without-shared --with-normal \
    --with-default-terminfo-dir=/usr/share/terminfo \
    --with-terminfo-dirs=/etc/terminfo:/lib/terminfo:/usr/share/terminfo \
    --enable-pc-files --with-pkg-config-libdir=/usr/lib/pkgconfig \
    --with-fallbacks=xterm,xterm-256color,vt100,linux,screen,tmux-256color
  make -j$JOBS
  both install.libs install.includes
  make install.progs DESTDIR=$DEST
  # the names configure scripts probe for (-ltinfo, -lncurses, -lcurses)
  for d in $DEST $SYSROOT; do
    for l in tinfo ncurses curses; do ln -sf lib${l}w.a $d/usr/lib/lib$l.a; done
    ln -sf libncursesw.a $d/usr/lib/libcurses.a
    # <ncursesw/ncurses.h> as Debian's libncurses-dev has it
    ln -sfn . $d/usr/include/ncursesw
  done
}

build_readline() {
  at_configure --with-curses --disable-shared --enable-static
  make -j$JOBS
  both install-static install-headers
  for d in $DEST $SYSROOT; do mkdir -p $d/usr/lib/pkgconfig; cp readline.pc $d/usr/lib/pkgconfig/; done
}

build_pcre2() {
  at_configure --enable-pcre2-16 --enable-pcre2-32 --disable-jit --enable-static --disable-shared
  make -j$JOBS
  both install; nola
}

build_bash() {
  # function pointer casts in unwind-protect (docs/gexcc.md §3); Debian's
  # system-wide bashrc and logout files
  export GEXCC_FPCAST=1
  at_configure --without-bash-malloc --disable-nls --with-curses --enable-largefile \
    CFLAGS="-O2 -Wno-implicit-function-declaration -DSYS_BASHRC='\"/etc/bash.bashrc\"' -DSYS_BASH_LOGOUT='\"/etc/bash.bash_logout\"' -DNON_INTERACTIVE_LOGIN_SHELLS"
  make -j$JOBS
  make install DESTDIR=$DEST
  mkdir -p $DEST/etc/skel
  cp debian/etc.bash.bashrc $DEST/etc/bash.bashrc
  cp debian/skel.bashrc $DEST/etc/skel/.bashrc; cp debian/skel.profile $DEST/etc/skel/.profile
  cp debian/skel.bash_logout $DEST/etc/skel/.bash_logout
  rm -rf $DEST/usr/share/info $DEST/usr/share/doc/bash
}

build_dash() {
  at_configure
  make -j$JOBS
  make install DESTDIR=$DEST
}

build_coreutils() {
  # one binary, the commands as symlinks (each program would otherwise carry
  # its own copy of libc and gnulib)
  FORCE_UNSAFE_CONFIGURE=1 at_configure --disable-nls --without-selinux --without-libgmp \
    --enable-single-binary=symlinks --disable-acl --disable-xattr --without-openssl \
    --enable-no-install-program=kill,uptime,hostname,groups
  make -j$JOBS
  make install DESTDIR=$DEST
  rm -rf $DEST/usr/share/info
}

build_grep() { at_configure --disable-nls; make -j$JOBS; make install DESTDIR=$DEST; rm -rf $DEST/usr/share/info; }
build_sed() { at_configure --disable-nls --without-selinux --disable-acl; make -j$JOBS; make install DESTDIR=$DEST; rm -rf $DEST/usr/share/info; }
build_gawk() {
  # Debian's (DFSG) source has no Texinfo manual sources: build without doc/,
  # and without extension/ (gawk's dynamic extensions cannot load here)
  at_configure --disable-nls --without-mpfr --with-readline
  make -j$JOBS SUBDIRS="support ."
  make install-exec SUBDIRS="support ." DESTDIR=$DEST
  mkdir -p $DEST/usr/share/man/man1; cp doc/gawk.1 $DEST/usr/share/man/man1/
  rm -rf $DEST/usr/include $DEST/usr/lib $DEST/usr/libexec
}
build_findutils() { at_configure --disable-nls --without-selinux; make -j$JOBS; make install DESTDIR=$DEST; rm -rf $DEST/usr/share/info; }
build_diffutils() { at_configure --disable-nls; make -j$JOBS; make install DESTDIR=$DEST; rm -rf $DEST/usr/share/info; }
build_tar() {
  FORCE_UNSAFE_CONFIGURE=1 at_configure --disable-nls --without-selinux --disable-acl --without-xattrs \
    --with-rmt=/usr/sbin/rmt
  make -j$JOBS
  make install DESTDIR=$DEST
  rm -rf $DEST/usr/share/info $DEST/usr/libexec
}
build_gzip() { at_configure; make -j$JOBS; make install DESTDIR=$DEST; rm -rf $DEST/usr/share/info; }
build_less() {
  CPPFLAGS="-I$SYSROOT/usr/include/ncursesw" at_configure --with-regex=pcre2 --with-secure=no
  make -j$JOBS
  make install DESTDIR=$DEST
  cp debian/lesspipe $DEST/usr/bin/lesspipe 2>/dev/null || true
}
build_debianutils() {
  # (po4a: translated man pages, which need po4a on the host; left out)
  # dh autoreconfs: the source has configure.ac only
  autoreconf -fi
  at_configure
  make -j$JOBS SUBDIRS=
  make install SUBDIRS= DESTDIR=$DEST
}
build_hostname() {
  make -j$JOBS CC=gexcc
  mkdir -p $DEST/usr/bin
  cp hostname $DEST/usr/bin/
  for n in dnsdomainname domainname nisdomainname ypdomainname; do ln -sf hostname $DEST/usr/bin/$n; done
}
build_nano() {
  # gnulib's strcasecmp check, as it runs on gex (musl's works)
  export gl_cv_func_strcasecmp_works=yes
  at_configure --disable-nls --enable-utf8 --disable-speller
  make -j$JOBS
  make install DESTDIR=$DEST
  rm -rf $DEST/usr/share/info
  mkdir -p $DEST/etc; cp doc/sample.nanorc $DEST/etc/nanorc 2>/dev/null || true
}

build_mawk() {
  at_configure
  make -j$JOBS
  mkdir -p $DEST/usr/bin; cp mawk $DEST/usr/bin/
}

build_sysvinit() {
  # sysvinit-utils only (Essential): killall5/pidof and fstab-decode
  make -C src -j$JOBS CC=gexcc killall5 fstab-decode
  mkdir -p $DEST/usr/bin $DEST/usr/sbin
  cp src/killall5 $DEST/usr/sbin/; ln -sf ../sbin/killall5 $DEST/usr/bin/pidof
  cp src/fstab-decode $DEST/usr/bin/
}

build_procps() {
  # Debian: watch with 8-bit characters, no systemd/elogind
  at_configure --disable-nls --without-systemd --without-elogind --disable-shared --enable-static \
    --enable-watch8bit --disable-pidwait --disable-modern-top \
    CPPFLAGS="-I$SYSROOT/usr/include/ncursesw"
  make -j$JOBS
  make install DESTDIR=$DEST
  rm -rf $DEST/usr/include $DEST/usr/lib/pkgconfig $DEST/usr/lib/*.a $DEST/usr/share/doc
}

build_file() {
  # cross-compiling the magic database needs a host `file` of the same
  # version
  rm -rf ../file-host; cp -a . ../file-host
  (cd ../file-host && CONFIG_SITE= CC=cc ./configure --build=x86_64-pc-linux-gnu --disable-shared --disable-libseccomp -q && make -j$JOBS -C src >/dev/null)
  at_configure --disable-shared --enable-static --disable-libseccomp --disable-bzlib --disable-xzlib --disable-zstdlib --disable-lzlib
  make -j$JOBS FILE_COMPILE=$PWD/../file-host/src/file
  make install DESTDIR=$DEST FILE_COMPILE=$PWD/../file-host/src/file
  rm -rf $DEST/usr/include $DEST/usr/lib/*.a $DEST/usr/lib/pkgconfig
  rm -rf ../file-host
}

build_vim() {
  # Debian's `vim` binary package (vim.basic); vim-common and vim-runtime
  # are Architecture: all and come from Debian. Cross configure: the
  # answers its run-time checks need (vim's own variables)
  export vim_cv_toupper_broken=no vim_cv_terminfo=yes vim_cv_tgetent=zero vim_cv_getcwd_broken=no \
    vim_cv_stat_ignores_slash=no vim_cv_memmove_handles_overlap=yes vim_cv_tty_group=world \
    vim_cv_tty_mode=0620 vim_cv_bcopy_handles_overlap=yes vim_cv_memcpy_handles_overlap=yes \
    ac_cv_small_wchar_t=no ac_cv_sizeof_int=4 ac_cv_sizeof_long=4 ac_cv_sizeof_time_t=8 ac_cv_sizeof_off_t=8
  cd src
  at_configure --with-features=huge --enable-gui=no --without-x --disable-gpm --disable-selinux \
    --disable-nls --disable-canberra --disable-libsodium --with-tlib=tinfo \
    --with-compiledby='gex (gexcc)' --with-modified-by='gex' \
    --with-vim-name=vim.basic --with-ex-name=ex.vim --with-view-name=view.vim
  make -j$JOBS
  mkdir -p $DEST/usr/bin
  cp vim.basic $DEST/usr/bin/vim.basic
}

build_util_linux() {
  # dh autoreconfs (Debian's patches touch Makefile.am); no autopoint: nls is off
  AUTOPOINT=true GTKDOCIZE=true autoreconf -fi
  # the commands a shell user reaches for; nothing needing devices,
  # mounts, logins or systemd. Debian splits them across util-linux,
  # bsdutils, bsdextrautils (and uuid-runtime).
  at_configure --disable-all-programs --disable-nls --without-systemd --without-udev --without-python \
    --without-selinux --without-audit --without-libmagic --disable-shared --enable-static \
    --enable-libuuid --enable-libsmartcols --enable-libblkid \
    --enable-more --enable-column --enable-hexdump --enable-getopt --enable-script --enable-scriptreplay \
    --enable-logger --enable-renice --enable-setsid --enable-flock --enable-rev --enable-look --enable-col \
    --enable-colcrt --enable-colrm --enable-ul --enable-cal --enable-mcookie --enable-whereis --enable-uuidgen \
    --enable-lscpu --enable-mesg --enable-waitpid --enable-cal \
    CPPFLAGS="-I$SYSROOT/usr/include/ncursesw"
  make -j$JOBS
  make install DESTDIR=$DEST
  rm -rf $DEST/usr/include $DEST/usr/lib/*.a $DEST/usr/lib/pkgconfig $DEST/usr/share/doc
}
