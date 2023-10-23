MEMORY
{
  /* NOTE 1 K = 1 KiBi = 1024 bytes */
  BOOT2      : ORIGIN = 0x10000000, LENGTH = 0x100
  FLASH      : ORIGIN = 0x10000100, LENGTH = 0x60000 - LENGTH(BOOT2)
  SHARED_RAM : ORIGIN = 0x2003F000, LENGTH = 0x1000

  RAM        : ORIGIN = 0x20000000,
               LENGTH = 256k - (LENGTH(SHARED_RAM))
}

SECTIONS {
  . = ORIGIN(SHARED_RAM);
  PROVIDE(_log_buffer = .);

  /* Consume remaining shared ram */
  . = ORIGIN(SHARED_RAM) + LENGTH(SHARED_RAM);
  PROVIDE(_log_buffer_end = .);
}

SECTIONS
{
  .rodata.gnu_build_id :
  {
    PROVIDE(g_note_build_id = .);
    *(.note.gnu.build-id)
  } > FLASH
} INSERT AFTER .rodata;
