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
  cp bzip2 bzip2recover bzgrep bzmore bzdiff debian/bzexe $DEST/usr/bin/
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
  cp programs/zstd programs/zstdgrep programs/zstdless $DEST/usr/bin/
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
  ln -sf bash $DEST/usr/bin/rbash
  mkdir -p $DEST/usr/share/debianutils/shells.d
  printf '/bin/bash\n/bin/rbash\n/usr/bin/bash\n/usr/bin/rbash\n' > $DEST/usr/share/debianutils/shells.d/bash
  rm -rf $DEST/usr/share/info $DEST/usr/share/doc/bash
}

build_dash() {
  at_configure
  make -j$JOBS
  make install DESTDIR=$DEST
  # trixie's dash ships /bin/sh itself
  ln -sf dash $DEST/usr/bin/sh
  mkdir -p $DEST/usr/share/debianutils/shells.d
  printf '/bin/dash\n/usr/bin/dash\n' > $DEST/usr/share/debianutils/shells.d/dash
}

build_coreutils() {
  # one binary, the commands as symlinks (each program would otherwise carry
  # its own copy of libc and gnulib)
  FORCE_UNSAFE_CONFIGURE=1 at_configure --disable-nls --without-selinux --without-libgmp \
    --enable-single-binary=symlinks --disable-acl --disable-xattr --without-openssl \
    --enable-install-program=arch --enable-no-install-program=kill,uptime,hostname
  make -j$JOBS
  make install DESTDIR=$DEST
  # Debian: chroot in sbin
  mkdir -p $DEST/usr/sbin; rm -f $DEST/usr/bin/chroot; ln -s ../bin/coreutils $DEST/usr/sbin/chroot
  rm -rf $DEST/usr/share/info
}

build_grep() { at_configure --disable-nls; make -j$JOBS; make install DESTDIR=$DEST; cp debian/rgrep $DEST/usr/bin/rgrep; chmod 755 $DEST/usr/bin/rgrep; rm -rf $DEST/usr/share/info; }
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
  ln -sf lesspipe $DEST/usr/bin/lessfile
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
  # the database where Debian's libmagic-mgc (which ours depends on) has it
  make -C src clean; make -j$JOBS -C src MAGIC=/usr/lib/file/magic
  make install DESTDIR=$DEST FILE_COMPILE=$PWD/../file-host/src/file
  rm -rf $DEST/usr/share/misc $DEST/usr/lib
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
  # Every program by default (most have no --enable switch, and
  # --disable-all-programs turns those off for good), minus what needs
  # mounts, logins, block devices or systemd. Debian splits the rest across
  # util-linux, bsdutils, bsdextrautils (and uuid-runtime).
  at_configure --bindir=/usr/bin --sbindir=/usr/sbin --libdir=/usr/lib \
    --disable-nls --without-systemd --without-udev --without-python --without-selinux --without-audit \
    --without-libmagic --disable-shared --enable-static --disable-makeinstall-chown --disable-makeinstall-setuid \
    --disable-poman --disable-asciidoc --disable-libmount --disable-libfdisk --disable-liblastlog2 --disable-pam-lastlog2 \
    --disable-login --disable-su --disable-runuser --disable-sulogin --disable-nologin --disable-chfn-chsh --disable-newgrp \
    --disable-vipw --disable-lslogins --disable-utmpdump --disable-last --disable-mount --disable-losetup --disable-fsck \
    --disable-swapon --disable-agetty --disable-hwclock --disable-cramfs --disable-bfs --disable-minix --disable-fdisks \
    --disable-partx --disable-eject --disable-lsfd --disable-irqtop --disable-lsirq --disable-zramctl --disable-wipefs \
    --disable-mkfs --disable-fstrim --disable-switch_root --disable-pivot_root --disable-unshare --disable-nsenter \
    --disable-setpriv --disable-lsns --disable-fallocate --disable-uuidd --disable-wdctl \
    CPPFLAGS="-I$SYSROOT/usr/include/ncursesw"
  make -j$JOBS
  make install DESTDIR=$DEST RANLIB=gexcc-ranlib
  # kill is procps's in Debian, rename the perl one's; the sbin tools are
  # for devices the guest does not have
  ln -sf hexdump $DEST/usr/bin/hd
  rm -rf $DEST/usr/include $DEST/usr/lib $DEST/usr/share/doc $DEST/usr/sbin $DEST/usr/bin/kill $DEST/usr/bin/rename \
    $DEST/usr/share/man/man8 $DEST/usr/share/man/man1/kill.1 $DEST/usr/share/man/man1/rename.1
}

# ---- tier 2

build_make_dfsg() {
  # the dfsg tarball has no doc/ (the manual is non-free): out of the build,
  # then the build system regenerated, as dh does
  sed -i 's| doc/Makefile||' configure.ac
  sed -i '/^SUBDIRS/s/ doc//' Makefile.am
  AUTOPOINT=true autoreconf -fi
  at_configure --disable-nls --without-guile
  make -j$JOBS
  make install DESTDIR=$DEST
  rm -rf $DEST/usr/include
}

# A tclsh for the build machine: SQLite's full source tree generates its
# amalgamation and keyword tables with Tcl scripts. Debian's tcl8.6, built
# once into $GEXCC_DEB/host.
host_tclsh() {
  local h=$GEXCC_DEB/host
  if [ ! -x $h/bin/tclsh8.6 ]; then
    python3 -I "$HERE/debsrc.py" --into $h tcl8.6=8.6.16+dfsg-1 > /dev/null
    (cd $h/tcl8.6-8.6.16+dfsg-1/unix && CONFIG_SITE= CC=cc ./configure -q --prefix=$h --disable-shared \
      && make -j$JOBS > /dev/null && make install-binaries install-libraries > /dev/null)
    ln -sf tclsh8.6 $h/bin/tclsh
  fi
  export PATH=$h/bin:$PATH
}

build_sqlite3() {
  host_tclsh
  # Debian's CFLAGS: the extensions its sqlite3 has (FTS, R*Tree, JSON is
  # built in, column metadata for python)
  at_configure --disable-tcl --enable-readline --disable-shared --enable-static --enable-fts5 \
    CFLAGS="-O2 -DSQLITE_ENABLE_FTS3 -DSQLITE_ENABLE_FTS3_PARENTHESIS -DSQLITE_ENABLE_RTREE -DSQLITE_ENABLE_COLUMN_METADATA -DSQLITE_ENABLE_DBSTAT_VTAB -DSQLITE_SECURE_DELETE -DSQLITE_ENABLE_UNLOCK_NOTIFY -DSQLITE_MAX_VARIABLE_NUMBER=250000" \
    CPPFLAGS="-I$SYSROOT/usr/include/ncursesw" LIBS="-lncursesw"
  make -j$JOBS
  both install; nola
  rm -rf $DEST/usr/include $DEST/usr/lib
}

build_libonig() {
  [ -x configure ] || autoreconf -fi
  at_configure --disable-shared --enable-static
  make -j$JOBS
  both install; nola
}

build_jq() {
  autoreconf -fi
  at_configure --with-oniguruma=$SYSROOT/usr --disable-docs --disable-maintainer-mode --disable-shared --enable-static \
    --disable-valgrind
  make -j$JOBS
  mkdir -p $DEST/usr/bin; cp jq $DEST/usr/bin/
}

build_lua5_4() {
  # Debian's makefile builds liblua5.4.so with libtool; here the same
  # objects, static: the POSIX build plus readline, as Debian's lua5.4 has
  # what debian/rules generates: where Debian puts C modules
  echo '#define DEB_HOST_MULTIARCH "wasm32-linux-musl"' > src/lua5.4-deb-multiarch.h
  cd src
  local f="-O2 -std=gnu99 -DLUA_COMPAT_5_3 -DLUA_USE_POSIX -DLUA_USE_READLINE -I$SYSROOT/usr/include/ncursesw"
  local core="lapi lcode lctype ldebug ldo ldump lfunc lgc llex lmem lobject lopcodes lparser lstate lstring ltable ltm lundump lvm lzio lauxlib lbaselib lcorolib ldblib liolib lmathlib loadlib loslib lstrlib ltablib lutf8lib linit"
  for o in $core lua luac; do gexcc $f -c $o.c -o $o.o & done; wait
  gexcc-ar rcs liblua.a $(for o in $core; do echo $o.o; done)
  gexcc -o lua lua.o liblua.a -lreadline -lncursesw -ltinfo -lm
  gexcc -o luac luac.o liblua.a -lm
  cd ..
  mkdir -p $DEST/usr/bin $SYSROOT/usr/lib $SYSROOT/usr/include/lua5.4
  cp src/lua $DEST/usr/bin/lua5.4; cp src/luac $DEST/usr/bin/luac5.4
  # the names Debian's alternatives give them
  ln -sf lua5.4 $DEST/usr/bin/lua; ln -sf luac5.4 $DEST/usr/bin/luac
  cp src/liblua.a $SYSROOT/usr/lib/liblua5.4.a
  cp src/lua.h src/luaconf.h src/lualib.h src/lauxlib.h src/lua.hpp $SYSROOT/usr/include/lua5.4/
}

build_tree() {
  make -j$JOBS CC=gexcc CFLAGS="-O2 -std=c11 -D_DEFAULT_SOURCE -D_FILE_OFFSET_BITS=64 -D_LARGEFILE64_SOURCE"
  mkdir -p $DEST/usr/bin; cp tree $DEST/usr/bin/
}

build_patch() {
  at_configure
  make -j$JOBS
  make install DESTDIR=$DEST
}

build_bc() {
  # bc's math library is compiled at build time by the bc just built: a host
  # bc of the same source makes libmath.h first
  # (the patched tree's timestamps ask for automake 1.14: regenerate)
  autoreconf -fi
  # libmath.h's last step is an ed script, and the build machine has no ed:
  # the same edit in sed
  cat > bc/fix-libmath_h <<'FIX'
sed -i -e '1s/^/{"/' -e 's/$/",/' -e '2,$s/^/"/' libmath.h
sed -i -e '$d' libmath.h
sed -i -e '$s/,$/,0}/' libmath.h
FIX
  rm -rf ../bc-host; cp -a . ../bc-host
  (cd ../bc-host && CONFIG_SITE= CC=cc ./configure -q --build=x86_64-pc-linux-gnu && make -j$JOBS MAKEINFO=true >/dev/null)
  at_configure --with-readline CPPFLAGS="-I$SYSROOT/usr/include/ncursesw" LIBS="-lncursesw -ltinfo"
  cp ../bc-host/bc/libmath.h bc/libmath.h
  make -j$JOBS MAKEINFO=true fbc_EXEEXT= || true
  touch bc/libmath.h
  make -j$JOBS MAKEINFO=true
  make install DESTDIR=$DEST MAKEINFO=true
  rm -rf $DEST/usr/share/info
}

build_ed() {
  ./configure --prefix=/usr --mandir=/usr/share/man --infodir=/usr/share/info CC=gexcc CFLAGS=-O2
  make -j$JOBS
  make install DESTDIR=$DEST
  rm -rf $DEST/usr/share/info
}

build_libevent() {
  # Debian's patches touch the build system (dh autoreconfs)
  autoreconf -fi
  at_configure --disable-shared --enable-static --disable-openssl --disable-samples --disable-libevent-regress \
    --disable-debug-mode
  make -j$JOBS
  both install; nola
}

build_tmux() {
  PKG_CONFIG=gexcc-pkg-config at_configure --disable-utf8proc CPPFLAGS="-I$SYSROOT/usr/include/ncursesw"
  make -j$JOBS
  make install DESTDIR=$DEST
}

build_htop() {
  [ -x configure ] || autoreconf -fi
  PKG_CONFIG=gexcc-pkg-config at_configure --enable-unicode --disable-sensors --disable-capabilities \
    --disable-hwloc --disable-delayacct --disable-unwind CPPFLAGS="-I$SYSROOT/usr/include/ncursesw"
  make -j$JOBS
  make install DESTDIR=$DEST
  rm -rf $DEST/usr/share/icons $DEST/usr/share/pixmaps
}

build_expat() {
  # the git tree (Debian's orig): the library in expat/, no configure
  cd expat
  ./buildconf.sh
  at_configure --without-docbook --without-examples --without-tests --disable-shared --enable-static
  make -j$JOBS
  both install; nola
  rm -rf $DEST
  mkdir -p $DEST
}

build_lz4() {
  make -j$JOBS -C lib CC=gexcc AR=gexcc-ar liblz4.a
  make -j$JOBS -C programs CC=gexcc AR=gexcc-ar lz4
  for d in $DEST $SYSROOT; do
    make -C lib install-static install-includes install-pc DESTDIR=$d PREFIX=/usr LIBDIR=/usr/lib 2>/dev/null \
      || { mkdir -p $d/usr/lib $d/usr/include; cp lib/liblz4.a $d/usr/lib/; cp lib/lz4.h lib/lz4hc.h lib/lz4frame.h $d/usr/include/; }
  done
  rm -rf $DEST/usr/lib $DEST/usr/include
  mkdir -p $DEST/usr/bin; cp programs/lz4 $DEST/usr/bin/
  for n in unlz4 lz4cat; do ln -sf lz4 $DEST/usr/bin/$n; done
}

build_mpdecimal() {
  at_configure --disable-shared --enable-static --disable-cxx
  make -j$JOBS
  both install; nola
  rm -rf $DEST; mkdir -p $DEST
}

build_python3_13() {
  # A native-style configure, as the corpus's: its checks and the build's
  # own steps (freezing, the stdlib's bytecode) run the programs just built,
  # under gexc. Extension modules static (no shared objects). Debian's
  # patches give its layout (dist-packages).
  export ac_cv_file__dev_ptmx=yes ac_cv_file__dev_ptc=no
  MODULE_BUILDTYPE=static PKG_CONFIG=gexcc-pkg-config CC=gexcc CXX=gexc++ AR=gexcc-ar RANLIB=gexcc-ranlib \
    CPPFLAGS="-I$SYSROOT/usr/include/ncursesw" \
    ./configure --prefix=/usr --disable-shared --without-ensurepip --disable-test-modules \
      --with-system-expat --with-system-libmpdec --with-pkg-config=yes --with-readline=readline \
      --without-static-libpython --with-openssl=$SYSROOT/usr
  make -j$JOBS
  make install DESTDIR=$DEST
  ln -sf python3.13 $DEST/usr/bin/python3
  # extension modules are built in; the directory still says where exec_prefix is
  mkdir -p $DEST/usr/lib/python3.13/lib-dynload
  rm -rf $DEST/usr/lib/python3.13/test $DEST/usr/lib/python3.13/idlelib/idle_test \
    $DEST/usr/lib/python3.13/config-3.13-* $DEST/usr/lib/*.a $DEST/usr/include
  find $DEST -name '*.opt-1.pyc' -o -name '*.opt-2.pyc' | xargs rm -f
}

build_openssl() {
  # no assembly (wasm32), static, Debian's openssldir
  ./Configure linux-generic32 no-asm no-shared no-dso no-tests no-docs no-afalgeng no-async \
    --prefix=/usr --libdir=lib --openssldir=/usr/lib/ssl CC=gexcc AR=gexcc-ar RANLIB=gexcc-ranlib \
    -DOPENSSL_NO_SECURE_MEMORY
  make -j$JOBS build_sw
  make install_sw DESTDIR=$SYSROOT
  make install_sw install_ssldirs DESTDIR=$DEST
  # the openssl binary; the libraries live in the programs that link them
  rm -rf $DEST/usr/include $DEST/usr/lib/*.a $DEST/usr/lib/pkgconfig $DEST/usr/lib/cmake $DEST/usr/bin/c_rehash
  # Debian's /usr/lib/ssl points at /etc/ssl (ca-certificates' bundle)
  rm -rf $DEST/usr/lib/ssl/certs $DEST/usr/lib/ssl/private $DEST/usr/lib/ssl/openssl.cnf*
  ln -sf /etc/ssl/certs $DEST/usr/lib/ssl/certs
  ln -sf /etc/ssl/private $DEST/usr/lib/ssl/private
  mkdir -p $DEST/etc/ssl; cp apps/openssl.cnf $DEST/etc/ssl/openssl.cnf
  ln -sf /etc/ssl/openssl.cnf $DEST/usr/lib/ssl/openssl.cnf
}

build_curl() {
  # OpenSSL, as Debian's curl; the CA bundle ca-certificates writes
  PKG_CONFIG=gexcc-pkg-config at_configure --with-openssl=$SYSROOT/usr --with-zlib=$SYSROOT/usr \
    --without-libpsl --disable-ldap --without-brotli --with-zstd=$SYSROOT/usr --without-libidn2 --without-nghttp2 \
    --without-libssh2 --without-librtmp --disable-shared --enable-static \
    --with-ca-bundle=/etc/ssl/certs/ca-certificates.crt --with-ca-path=/etc/ssl/certs LIBS="-lzstd"
  make -j$JOBS
  both install; nola
  rm -rf $DEST/usr/include $DEST/usr/lib $DEST/usr/bin/curl-config $DEST/usr/share/aclocal
}

build_git() {
  # git's own makefile; no Tcl/Tk (gitk, git-gui), gettext, Perl or Python
  # parts (send-email, svn, p4); https through libcurl and OpenSSL
  local m="prefix=/usr gitexecdir=/usr/lib/git-core CC=gexcc AR=gexcc-ar RANLIB=gexcc-ranlib uname_S=Linux uname_M=wasm32
    NO_TCLTK=1 NO_GETTEXT=1 NO_PERL=1 NO_PYTHON=1 NO_SVN_TESTS=1 USE_LIBPCRE2=1 NO_INSTALL_HARDLINKS=1
    CURL_CONFIG=$SYSROOT/usr/bin/curl-config CURL_LDFLAGS='-lcurl -lssl -lcrypto -lz -lzstd'
    EXPATDIR=$SYSROOT/usr ZLIB_PATH=$SYSROOT/usr NEEDS_LIBICONV= HAVE_DEV_TTY=1 NO_REGEX=NeedsStartEnd
    CFLAGS=-O2 V=1"
  eval make -j$JOBS $m all
  eval make $m install DESTDIR=$DEST
  rm -rf $DEST/usr/share/perl5 $DEST/usr/share/gitweb $DEST/usr/share/git-gui $DEST/usr/share/gitk
}
build_dpkg() {
  # Native architecture arm64 (docs/gexcc.md §6.1a): configure would ask
  # dpkg-architecture about the wasm32 host triplet; say arm64 instead
  sed -i 's|^\( *\)cpu_type=\$(PERL=.*$|\1cpu_type=arm64|; s|^\( *\)os_type=\$(PERL=.*$|\1os_type=linux|; s|^\( *\)dpkg_arch=\$(PERL=.*$|\1dpkg_arch=arm64|' configure
  at_configure --libexecdir=/usr/lib --disable-nls --disable-dselect --disable-devel-docs \
    --without-libselinux --with-libz --with-liblzma --with-libzstd --with-libbz2 \
    --disable-shared --enable-static PERL=/usr/bin/perl
  grep -q '#define ARCHITECTURE "arm64"' config.h
  make -j$JOBS -C lib
  make -j$JOBS -C src
  make -j$JOBS -C utils
  make -C src install DESTDIR=$DEST
  make -C utils install DESTDIR=$DEST
  # Debian's dpkg package (debian/dpkg.install), the C side
  mkdir -p $DEST/etc/dpkg/dpkg.cfg.d $DEST/etc/alternatives \
    $DEST/var/lib/dpkg/{alternatives,info,parts,updates} $DEST/usr/share/dpkg/sh
  cp debian/dpkg.cfg $DEST/etc/dpkg/
  cp data/*table $DEST/usr/share/dpkg/
  cp src/sh/dpkg-error.sh $DEST/usr/share/dpkg/sh/
  rm -rf $DEST/usr/share/polkit-1
}
build_xxhash() {
  make -j$JOBS CC=gexcc AR=gexcc-ar libxxhash.a
  mkdir -p $SYSROOT/usr/lib $SYSROOT/usr/include
  cp libxxhash.a $SYSROOT/usr/lib/; cp xxhash.h xxh3.h $SYSROOT/usr/include/
}
# apt's build runs triehash (a perl script) on the build machine
host_triehash() {
  local h=$GEXCC_DEB/host
  if [ ! -x $h/bin/triehash ]; then
    python3 -I "$HERE/debsrc.py" --into $h triehash=0.3-3 > /dev/null
    mkdir -p $h/bin; install -m755 $h/triehash-0.3-3/triehash.pl $h/bin/triehash
  fi
  export PATH=$h/bin:$PATH
}
build_apt() {
  # Native architecture arm64 like our dpkg (docs/gexcc.md §6.1a). Static:
  # libapt-pkg and libapt-private go into each program. No seccomp, udev,
  # systemd, berkeley db (apt-ftparchive only), nls or docs. No sqv: the
  # gpgv method runs our native gpgv.
  host_triehash
  # (Size is 64-bit in FileFd, size_t 32-bit: ZSTD buffers narrow it)
  # without NLS apt defines setlocale and friends inline with throw(); musl
  # declares them without, which C++ rejects
  sed -i '/^extern "C" inline/s/ throw()//' CMake/apti18n.h.in
  sed -i 's/add_library(apt-pkg SHARED/add_library(apt-pkg STATIC/; /-Wl,-version-script=/d' apt-pkg/CMakeLists.txt
  sed -i 's/add_library(apt-private SHARED/add_library(apt-private STATIC/' apt-private/CMakeLists.txt
  sed -i 's/^find_package(Berkeley REQUIRED)/find_package(Berkeley)/; s/^add_subdirectory(ftparchive)/#&/; s/^add_subdirectory(test)/#&/; s/^add_subdirectory(doc)/#&/; s/^add_subdirectory(dselect)/#&/' CMakeLists.txt
  mkdir -p build && cd build
  cmake .. -G "Unix Makefiles" -DCMAKE_TOOLCHAIN_FILE=$GEXCC_PREFIX/share/gexcc/toolchain.cmake \
    -DCMAKE_INSTALL_PREFIX=/usr -DCMAKE_INSTALL_SYSCONFDIR=/etc -DCMAKE_INSTALL_LOCALSTATEDIR=/var \
    -DCMAKE_INSTALL_LIBDIR=lib -DCMAKE_INSTALL_LIBEXECDIR=lib -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_EXPORT_COMPILE_COMMANDS=ON \
    -DCMAKE_STAGING_PREFIX= \
    -DCMAKE_CXX_FLAGS="-Wno-c++11-narrowing" -DWITH_DOC=OFF -DWITH_TESTS=OFF -DUSE_NLS=OFF -DCOMMON_ARCH=arm64 -DDPKG_DATADIR=/usr/share/dpkg \
    -DROOT_GROUP=root -DSQV_EXECUTABLE=SQV_EXECUTABLE-NOTFOUND \
    -DSECCOMP_FOUND=OFF -DUDEV_FOUND=OFF -DSYSTEMD_FOUND=OFF -DBERKELEY_FOUND=OFF \
    -DCMAKE_DISABLE_FIND_PACKAGE_SECCOMP=ON -DCMAKE_DISABLE_FIND_PACKAGE_Udev=ON \
    -DCMAKE_DISABLE_FIND_PACKAGE_Systemd=ON -DCMAKE_DISABLE_FIND_PACKAGE_Berkeley=ON
  make -j$JOBS
  make install DESTDIR=$DEST
  cd ..
  # (the toolchain's staging prefix, emptied above, also empties the install
  # prefix for the /usr parts; the compiled-in paths are /usr's)
  mkdir -p $DEST/usr
  for d in bin lib share include; do if [ -d $DEST/$d ]; then cp -a $DEST/$d $DEST/usr/; rm -rf $DEST/$d; fi; done
  # Debian's apt package (debian/apt.install): the programs, methods, config
  rm -rf $DEST/usr/include $DEST/usr/lib/*.a $DEST/usr/lib/pkgconfig $DEST/usr/share/doc
  rm -f $DEST/usr/bin/apt-extracttemplates $DEST/usr/bin/apt-ftparchive $DEST/usr/bin/apt-sortpkgs
  # one program for all of apt's (aptmulti.py): 15 MB instead of 100
  python3 -I "$HERE/aptmulti.py" $PWD/build $PWD/build/apt-multi > build/multi-names
  rm -f $DEST/usr/bin/apt*; rm -rf $DEST/usr/lib/apt/methods $DEST/usr/lib/apt/solvers $DEST/usr/lib/apt/planners \
    $DEST/usr/lib/apt/apt-helper $DEST/usr/lib/apt/apt-extracttemplates
  install -m755 build/apt-multi $DEST/usr/lib/apt/apt-multi
  while read -r n; do
    case $n in
      bin/*) ln -s ../lib/apt/apt-multi $DEST/usr/$n ;;
      */*) mkdir -p $DEST/usr/lib/apt/${n%/*}; ln -s ../apt-multi $DEST/usr/lib/apt/$n ;;
      *) ln -s apt-multi $DEST/usr/lib/apt/$n ;;
    esac
  done < build/multi-names
  mkdir -p $DEST/etc/apt/{apt.conf.d,preferences.d,sources.list.d,trusted.gpg.d,keyrings,auth.conf.d} \
    $DEST/var/lib/apt/{lists/partial,periodic,mirrors/partial} $DEST/var/cache/apt/archives/partial $DEST/var/log/apt
  cp debian/apt.conf.autoremove $DEST/etc/apt/apt.conf.d/01autoremove 2>/dev/null || true
}
# the GnuPG libraries' configure scripts find each other through gpgrt-config
# and the .pc files in the sysroot (gpg-error-config is not installed)
export GPGRT_CONFIG=$SYSROOT/usr/bin/gpgrt-config
build_libgpg_error() {
  at_configure --enable-static --disable-shared --disable-nls --disable-doc --disable-tests
  make -j$JOBS
  both install; nola
}
build_libgcrypt20() {
  # no assembly (wasm32); the generic C code paths
  at_configure --enable-static --disable-shared --disable-asm --disable-doc --disable-jent-support \
    --disable-padlock-support --disable-aesni-support --disable-shaext-support --disable-pclmul-support \
    --disable-sse41-support --disable-drng-support --disable-avx-support --disable-avx2-support \
    --disable-neon-support --disable-arm-crypto-support
  make -j$JOBS -C cipher; make -j$JOBS -C random; make -j$JOBS -C mpi; make -j$JOBS -C compat; make -j$JOBS -C src
  make -C src install DESTDIR=$SYSROOT; nola
}
build_libassuan() {
  at_configure --enable-static --disable-shared --disable-doc
  make -j$JOBS -C src
  make -C src install DESTDIR=$SYSROOT; nola
}
build_libksba() {
  at_configure --enable-static --disable-shared --disable-doc
  make -j$JOBS -C gl; make -j$JOBS -C src
  make -C src install DESTDIR=$SYSROOT; nola
}
build_npth() {
  # (gnupg's configure requires it; gpgv itself does not use threads)
  at_configure --enable-static --disable-shared --disable-tests
  make -j$JOBS -C src
  make install DESTDIR=$SYSROOT SUBDIRS=src; nola
}
build_gnupg2() {
  # gpgv only, configured as Debian's gpgv (udeb) build: what apt runs to
  # check InRelease against debian-archive-keyring
  at_configure --libexecdir=/usr/lib/gnupg --disable-doc --disable-nls --disable-bzip2 \
    --disable-gpgtar --disable-gpgsm --disable-scdaemon --disable-dirmngr --disable-tofu --disable-exec \
    --disable-ldap --disable-gnutls --disable-sqlite --disable-libdns --disable-keyboxd --disable-tpm2d \
    --disable-card-support --disable-ccid-driver --disable-wks-tools \
    --with-zlib=$SYSROOT/usr
  make -j$JOBS -C common; make -j$JOBS -C regexp; make -j$JOBS -C kbx libkeybox.a
  make -j$JOBS -C g10 gpgv
  mkdir -p $DEST/usr/bin; cp g10/gpgv $DEST/usr/bin/
}
