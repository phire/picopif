
INCLUDE memory.x

/* The entry point is the reset handler */
ENTRY(Start);

EXTERN(RESET_VECTOR);

EXTERN(DefaultHandler);

PROVIDE(NonMaskableInt = DefaultHandler);
EXTERN(HardFaultTrampoline);
PROVIDE(MemoryManagement = DefaultHandler);
PROVIDE(BusFault = DefaultHandler);
PROVIDE(UsageFault = DefaultHandler);
PROVIDE(SecureFault = DefaultHandler);
PROVIDE(SVCall = DefaultHandler);
PROVIDE(DebugMonitor = DefaultHandler);
PROVIDE(PendSV = DefaultHandler);
PROVIDE(SysTick = DefaultHandler);

PROVIDE(DefaultHandler = DefaultHandler_);
PROVIDE(HardFault = HardFault_);

/* # Interrupt vectors */
EXTERN(__INTERRUPTS); /* `static` variable similar to `__EXCEPTIONS` */

SECTIONS
{

  .vector_table : ALIGN(0x40)
  {
    __vector_table = .;

    /* SP when re-endering boot2 */
    LONG(ORIGIN(RAM) + LENGTH(RAM));

    /* Reset vector -- Re-enter boot2 on reset */
    LONG(ORIGIN(FLASH));
    __reset_vector = .;

    /* Exceptions */
    KEEP(*(.vector_table.exceptions)); /* this is the `__EXCEPTIONS` symbol */
    __eexceptions = .;

    /* Device specific interrupts */
    KEEP(*(.vector_table.interrupts)); /* this is the `__INTERRUPTS` symbol */
  } > APP_FLASH

  .text : ALIGN(4)
  {
    *(.text .text.*);

    /* The HardFaultTrampoline uses the `b` instruction to enter `HardFault`,
       so must be placed close to it. */
    *(.HardFaultTrampoline);
    *(.HardFault.*);

    . = ALIGN(16);
  } > APP_FLASH

  .rodata : ALIGN(4)
  {
    *(.rodata .rodata.*);
    PROVIDE(g_note_build_id = .);
    *(.note.gnu.build-id)

    . = ALIGN(4);
  } > APP_FLASH

  .data : ALIGN(4)
  {
    *(.data .data.*);
  } > APP_RAM AT>APP_FLASH

  .bss :
  {
    . = ALIGN(4);
    *(.bss .bss.*);
  } > APP_RAM

  /* ### .uninit */
  .uninit (NOLOAD) : ALIGN(4)
  {
    *(.uninit .uninit.*);
  } > APP_RAM

  /* Place the heap right after `.uninit` in RAM */
  PROVIDE(__sheap = __euninit);

  /DISCARD/ :
  {
    *(.ARM.exidx .ARM.exidx.*);
  }
}

INCLUDE device.x
