MEMORY
{
  /* NOTE 1 K = 1 KiBi = 1024 bytes */
  BOOT2      : ORIGIN = 0x10000000, LENGTH = 0x100
  FLASH      : ORIGIN = 0x10000100, LENGTH = 2m - LENGTH(BOOT2)
  RAM        : ORIGIN = 0x20000000, LENGTH = 256K

  /* Use SRAM 4 for persistent ram across boots. The bootrom uses the top of SRAM 5 as a stack
     And the memcpy boot2 overwriters all 256k of SRAM 0-3 */
  SHARED_RAM : ORIGIN = 0x20040000, LENGTH = 0x1000
}

SECTIONS {
  PROVIDE(BOOT2_FIRMWARE = ORIGIN(BOOT2));

  . = ORIGIN(SHARED_RAM);
  PROVIDE(_log_buffer = .);

  /* Consume remaining shared ram */
  . = ORIGIN(SHARED_RAM) + LENGTH(SHARED_RAM);
  PROVIDE(_log_buffer_end = .);
}
