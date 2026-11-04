/* hoststub.c as one flat, position-independent blob: _start at offset 0,
   the runner path (gexcc_runner, 256 bytes) last. */
SECTIONS {
  . = 0;
  .blob : {
    *(.text.start)
    *(.text .text.*)
    *(.rodata .rodata.*)
    *(.data .data.* .bss .bss.*)
    *(.got .got.plt)
    . = ALIGN(4);
    *(.text.zcfg)
  }
  /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.dynamic) *(.dynsym) *(.dynstr) *(.hash) *(.gnu.hash) }
}
