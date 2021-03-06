#![no_std]
#![no_main]
#![feature(asm)]
// Enable returning `!`
#![feature(never_type)]
#[allow(unused_extern_crates)]

#[cfg(not(feature = "semihosting"))]
extern crate panic_abort;
#[cfg(feature = "semihosting")]
extern crate panic_semihosting;

extern crate cortex_m;
extern crate cortex_m_rt;
extern crate stm32h7;

#[macro_use]
extern crate log;

use core::ptr;
use core::cell::RefCell;
use cortex_m_rt::{entry, exception};
use stm32h7::stm32h7x3::{self as stm32, Peripherals, CorePeripherals, interrupt};
use cortex_m::interrupt::Mutex;

mod iir;
use iir::*;

#[cfg(not(feature = "semihosting"))]
fn init_log() {}

#[cfg(feature = "semihosting")]
fn init_log() {
    use log::LevelFilter;
    use cortex_m_log::log::{Logger, init};
    use cortex_m_log::printer::semihosting::{InterruptOk, hio::HStdout};
    static mut LOGGER: Option<Logger<InterruptOk<HStdout>>> = None;
    let logger = Logger {
        inner: InterruptOk::<_>::stdout().expect("semihosting stdout"),
        level: LevelFilter::Info,
    };
    let logger = unsafe {
        LOGGER.get_or_insert(logger)
    };

    init(logger).expect("set logger");
}

// Pull in build information (from `built` crate)
mod build_info {
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

fn pwr_setup(pwr: &stm32::PWR) {
    // go to VOS1 voltage scale for high perf
    pwr.pwr_cr3.write(|w|
        w.sden().set_bit()
         .ldoen().set_bit()
         .bypass().clear_bit()
    );
    while pwr.pwr_csr1.read().actvosrdy().bit_is_clear() {}
    pwr.pwr_d3cr.write(|w| unsafe { w.vos().bits(0b11) });  // vos1
    while pwr.pwr_d3cr.read().vosrdy().bit_is_clear() {}
}

fn rcc_reset(rcc: &stm32::RCC) {
    // Reset all peripherals
    rcc.ahb1rstr.write(|w| unsafe { w.bits(0xFFFF_FFFF) });
    rcc.ahb1rstr.write(|w| unsafe { w.bits(0)});
    rcc.apb1lrstr.write(|w| unsafe { w.bits(0xFFFF_FFFF) });
    rcc.apb1lrstr.write(|w| unsafe { w.bits(0)});
    rcc.apb1hrstr.write(|w| unsafe { w.bits(0xFFFF_FFFF) });
    rcc.apb1hrstr.write(|w| unsafe { w.bits(0)});

    rcc.ahb2rstr.write(|w| unsafe { w.bits(0xFFFF_FFFF) });
    rcc.ahb2rstr.write(|w| unsafe { w.bits(0)});
    rcc.apb2rstr.write(|w| unsafe { w.bits(0xFFFF_FFFF) });
    rcc.apb2rstr.write(|w| unsafe { w.bits(0)});

    // do not reset the cpu
    rcc.ahb3rstr.write(|w| unsafe { w.bits(0x7FFF_FFFF) });
    rcc.ahb3rstr.write(|w| unsafe { w.bits(0)});
    rcc.apb3rstr.write(|w| unsafe { w.bits(0xFFFF_FFFF) });
    rcc.apb3rstr.write(|w| unsafe { w.bits(0)});

    rcc.ahb4rstr.write(|w| unsafe { w.bits(0xFFFF_FFFF) });
    rcc.ahb4rstr.write(|w| unsafe { w.bits(0)});
    rcc.apb4rstr.write(|w| unsafe { w.bits(0xFFFF_FFFF) });
    rcc.apb4rstr.write(|w| unsafe { w.bits(0)});
}

fn rcc_pll_setup(rcc: &stm32::RCC, flash: &stm32::FLASH) {
    // Ensure HSI is on and stable
    rcc.cr.modify(|_, w| w.hsion().set_bit());
    while rcc.cr.read().hsirdy().bit_is_clear() {}

    // Set system clock to HSI
    rcc.cfgr.modify(|_, w| unsafe { w.sw().bits(0) });  // hsi
    while rcc.cfgr.read().sws().bits() != 0 {}

    // Clear registers to reset value
    rcc.cr.write(|w| w.hsion().set_bit());
    rcc.cfgr.reset();

    // Ensure HSE is on and stable
    rcc.cr.modify(|_, w|
        w.hseon().set_bit()
         .hsebyp().clear_bit());
    while rcc.cr.read().hserdy().bit_is_clear() {}

    rcc.pllckselr.modify(|_, w| unsafe {
        w.pllsrc().bits(0b10)  // hse
         .divm1().bits(1)  // ref prescaler
         .divm2().bits(1)  // ref prescaler
    });
    // Configure PLL1: 8MHz /1 *100 /2 = 400 MHz
    rcc.pllcfgr.modify(|_, w| unsafe {
        w.pll1vcosel().clear_bit()  // 192-836 MHz VCO
         .pll1rge().bits(0b11)  // 8-16 MHz PFD
         .pll1fracen().clear_bit()
         .divp1en().set_bit()
         .pll2vcosel().set_bit()  // 150-420 MHz VCO
         .pll2rge().bits(0b11)  // 8-16 MHz PFD
         .pll2fracen().clear_bit()
         .divp2en().set_bit()
         .divq2en().set_bit()
    });
    rcc.pll1divr.write(|w| unsafe {
        w.divn1().bits(100 - 1)  // feebdack divider
         .divp1().bits(2 - 1)  // p output divider
    });
    rcc.cr.modify(|_, w| w.pll1on().set_bit());
    while rcc.cr.read().pll1rdy().bit_is_clear() {}

    // Configure PLL2: 8MHz /1 *25 / 2 = 100 MHz
    rcc.pll2divr.write(|w| unsafe {
        w.divn1().bits(25 - 1)  // feebdack divider
         .divp1().bits(2 - 1)  // p output divider
         .divq1().bits(2 - 1)  // q output divider
    });
    rcc.cr.modify(|_, w| w.pll2on().set_bit());
    while rcc.cr.read().pll2rdy().bit_is_clear() {}

    // hclk 200 MHz, pclk 100 MHz
    let dapb = 0b100;
    rcc.d1cfgr.write(|w| unsafe {
        w.d1cpre().bits(0)  // sys_ck not divided
         .hpre().bits(0b1000)  // rcc_hclk3 = sys_d1cpre_ck / 2
         .d1ppre().bits(dapb) // rcc_pclk3 = rcc_hclk3 / 2
    });
    rcc.d2cfgr.write(|w| unsafe {
        w.d2ppre1().bits(dapb)  // rcc_pclk1 = rcc_hclk3 / 2
         .d2ppre2().bits(dapb) // rcc_pclk2 = rcc_hclk3 / 2

    });
    rcc.d3cfgr.write(|w| unsafe {
        w.d3ppre().bits(dapb)  // rcc_pclk4 = rcc_hclk3 / 2
    });

    // 2 wait states, 0b10 programming delay
    // 185-210 MHz
    flash.acr.write(|w| unsafe {
        w.wrhighfreq().bits(2)
         .latency().bits(2)
    });
    while flash.acr.read().latency().bits() != 2 {}

    // CSI for I/O compensationc ell
    rcc.cr.modify(|_, w| w.csion().set_bit());
    while rcc.cr.read().csirdy().bit_is_clear() {}

    // Set system clock to pll1_p
    rcc.cfgr.modify(|_, w| unsafe { w.sw().bits(0b011) });  // pll1p
    while rcc.cfgr.read().sws().bits() != 0b011 {}

    rcc.d1ccipr.write(|w| unsafe {
        w.ckpersrc().bits(1)  // hse_ck
    });
    rcc.d2ccip1r.modify(|_, w| unsafe {
        w.spi123src().bits(1)  // pll2_p
         .spi45src().bits(1)  // pll2_q
    });

    rcc.d3ccipr.modify(|_, w| unsafe {
        w.spi6src().bits(1)  // pll2_q
    });
}

fn io_compensation_setup(syscfg: &stm32::SYSCFG) {
    syscfg.cccsr.modify(|_, w|
        w.en().set_bit()
         .cs().clear_bit()
         .hslv().clear_bit()
    );
    while syscfg.cccsr.read().ready().bit_is_clear() {}
}

fn gpio_setup(gpioa: &stm32::GPIOA, gpiob: &stm32::GPIOB, gpiod: &stm32::GPIOD,
              gpioe: &stm32::GPIOE, gpiof: &stm32::GPIOF, gpiog: &stm32::GPIOG) {
    // FP_LED0
    gpiod.otyper.modify(|_, w| w.ot5().push_pull());
    gpiod.moder.modify(|_, w| w.moder5().output());
    gpiod.odr.modify(|_, w| w.odr5().clear_bit());

    // FP_LED1
    gpiod.otyper.modify(|_, w| w.ot6().push_pull());
    gpiod.moder.modify(|_, w| w.moder6().output());
    gpiod.odr.modify(|_, w| w.odr6().clear_bit());

    // LED_FP2
    gpiog.otyper.modify(|_, w| w.ot4().push_pull());
    gpiog.moder.modify(|_, w| w.moder4().output());
    gpiog.odr.modify(|_, w| w.odr4().clear_bit());

    // LED_FP3
    gpiod.otyper.modify(|_, w| w.ot12().push_pull());
    gpiod.moder.modify(|_, w| w.moder12().output());
    gpiod.odr.modify(|_, w| w.odr12().clear_bit());

    // AFE0_A0,1: PG2,PG3
    gpiog.otyper.modify(|_, w|
        w.ot2().push_pull()
         .ot3().push_pull()
    );
    gpiog.moder.modify(|_, w|
        w.moder2().output()
         .moder3().output()
    );
    gpiog.odr.modify(|_, w|
        w.odr2().clear_bit()
         .odr3().clear_bit()
    );

    // ADC0
    // SCK: PG11
    gpiog.moder.modify(|_, w| w.moder11().alternate());
    gpiog.otyper.modify(|_, w| w.ot11().push_pull());
    gpiog.ospeedr.modify(|_, w| w.ospeedr11().very_high_speed());
    gpiog.afrh.modify(|_, w| w.afr11().af5());
    // MOSI: PD7
    // MISO: PA6
    gpioa.moder.modify(|_, w| w.moder6().alternate());
    gpioa.afrl.modify(|_, w| w.afr6().af5());
    // NSS: PG10
    gpiog.moder.modify(|_, w| w.moder10().alternate());
    gpiog.otyper.modify(|_, w| w.ot10().push_pull());
    gpiog.ospeedr.modify(|_, w| w.ospeedr10().very_high_speed());
    gpiog.afrh.modify(|_, w| w.afr10().af5());

    // DAC0
    // SCK: PB10
    gpiob.moder.modify(|_, w| w.moder10().alternate());
    gpiob.otyper.modify(|_, w| w.ot10().push_pull());
    gpiob.ospeedr.modify(|_, w| w.ospeedr10().very_high_speed());
    gpiob.afrh.modify(|_, w| w.afr10().af5());
    // MOSI: PB15
    gpiob.moder.modify(|_, w| w.moder15().alternate());
    gpiob.otyper.modify(|_, w| w.ot15().push_pull());
    gpiob.ospeedr.modify(|_, w| w.ospeedr15().very_high_speed());
    gpiob.afrh.modify(|_, w| w.afr15().af5());
    // MISO: PB14
    // NSS: PB9
    gpiob.moder.modify(|_, w| w.moder9().alternate());
    gpiob.otyper.modify(|_, w| w.ot9().push_pull());
    gpiob.ospeedr.modify(|_, w| w.ospeedr9().very_high_speed());
    gpiob.afrh.modify(|_, w| w.afr9().af5());

    // DAC0_LDAC: PE11
    gpioe.moder.modify(|_, w| w.moder11().output());
    gpioe.otyper.modify(|_, w| w.ot11().push_pull());
    gpioe.odr.modify(|_, w| w.odr11().clear_bit());

    // DAC_CLR: PE12
    gpioe.moder.modify(|_, w| w.moder12().output());
    gpioe.otyper.modify(|_, w| w.ot12().push_pull());
    gpioe.odr.modify(|_, w| w.odr12().set_bit());

    // AFE1_A0,1: PD14,PD15
    gpiod.otyper.modify(|_, w|
        w.ot14().push_pull()
         .ot15().push_pull()
    );
    gpiod.moder.modify(|_, w|
        w.moder14().output()
         .moder15().output()
    );
    gpiod.odr.modify(|_, w|
        w.odr14().clear_bit()
         .odr15().clear_bit()
    );

    // ADC1
    // SCK: PF6
    gpiof.moder.modify(|_, w| w.moder7().alternate());
    gpiof.otyper.modify(|_, w| w.ot7().push_pull());
    gpiof.ospeedr.modify(|_, w| w.ospeedr7().very_high_speed());
    gpiof.afrl.modify(|_, w| w.afr7().af5());
    // MOSI: PF9
    // MISO: PF7
    gpiof.moder.modify(|_, w| w.moder8().alternate());
    gpiof.afrh.modify(|_, w| w.afr8().af5());
    // NSS: PF8
    gpiof.moder.modify(|_, w| w.moder6().alternate());
    gpiof.otyper.modify(|_, w| w.ot6().push_pull());
    gpiof.ospeedr.modify(|_, w| w.ospeedr6().very_high_speed());
    gpiof.afrl.modify(|_, w| w.afr6().af5());

    // DAC1
    // SCK: PE2
    gpioe.moder.modify(|_, w| w.moder2().alternate());
    gpioe.otyper.modify(|_, w| w.ot2().push_pull());
    gpioe.ospeedr.modify(|_, w| w.ospeedr2().very_high_speed());
    gpioe.afrl.modify(|_, w| w.afr2().af5());
    // MOSI: PE6
    gpioe.moder.modify(|_, w| w.moder6().alternate());
    gpioe.otyper.modify(|_, w| w.ot6().push_pull());
    gpioe.ospeedr.modify(|_, w| w.ospeedr6().very_high_speed());
    gpioe.afrl.modify(|_, w| w.afr6().af5());
    // MISO: PE5
    // NSS: PE4
    gpioe.moder.modify(|_, w| w.moder4().alternate());
    gpioe.otyper.modify(|_, w| w.ot4().push_pull());
    gpioe.ospeedr.modify(|_, w| w.ospeedr4().very_high_speed());
    gpioe.afrl.modify(|_, w| w.afr4().af5());

    // DAC1_LDAC: PE15
    gpioe.moder.modify(|_, w| w.moder15().output());
    gpioe.otyper.modify(|_, w| w.ot15().push_pull());
    gpioe.odr.modify(|_, w| w.odr15().clear_bit());
}

// ADC0
fn spi1_setup(spi1: &stm32::SPI1) {
    spi1.cfg1.modify(|_, w| unsafe {
        w.mbr().bits(1)  // clk/4
         .dsize().bits(16 - 1)
         .fthvl().bits(1 - 1)  // one data
    });
    spi1.cfg2.modify(|_, w| unsafe {
        w.afcntr().set_bit()
         .ssom().set_bit()  // ss deassert between frames during midi
         .ssoe().set_bit()  // ss output enable
         .ssiop().clear_bit()  // ss active low
         .ssm().clear_bit()  // PAD counts
         .cpol().set_bit()
         .cpha().set_bit()
         .lsbfrst().clear_bit()
         .master().set_bit()
         .sp().bits(0)  // motorola
         .comm().bits(0b10)  // simplex receiver
         .ioswp().clear_bit()
         .midi().bits(0)  // master inter data idle
         .mssi().bits(6)  // master SS idle
    });
    spi1.cr2.modify(|_, w| unsafe {
        w.tsize().bits(1)
    });
    spi1.cr1.write(|w| w.spe().set_bit());
}

// ADC1
fn spi5_setup(spi5: &stm32::SPI5) {
    spi5.cfg1.modify(|_, w| unsafe {
        w.mbr().bits(1)  // clk/4
         .dsize().bits(16 - 1)
         .fthvl().bits(1 - 1)  // one data
    });
    spi5.cfg2.modify(|_, w| unsafe {
        w.afcntr().set_bit()
         .ssom().set_bit()  // ss deassert between frames during midi
         .ssoe().set_bit()  // ss output enable
         .ssiop().clear_bit()  // ss active low
         .ssm().clear_bit()  // PAD counts
         .cpol().set_bit()
         .cpha().set_bit()
         .lsbfrst().clear_bit()
         .master().set_bit()
         .sp().bits(0)  // motorola
         .comm().bits(0b10)  // simplex receiver
         .ioswp().clear_bit()
         .midi().bits(0)  // master inter data idle
         .mssi().bits(6)  // master SS idle
    });
    spi5.cr2.modify(|_, w| unsafe {
        w.tsize().bits(1)
    });
    spi5.cr1.write(|w| w.spe().set_bit());
}

// DAC0
fn spi2_setup(spi2: &stm32::SPI2) {
    spi2.cfg1.modify(|_, w| unsafe {
        w.mbr().bits(0)  // clk/2
         .dsize().bits(16 - 1)
         .fthvl().bits(1 - 1)  // one data
    });
    spi2.cfg2.modify(|_, w| unsafe {
        w.afcntr().set_bit()
         .ssom().set_bit()  // ss deassert between frames during midi
         .ssoe().set_bit()  // ss output enable
         .ssiop().clear_bit()  // ss active low
         .ssm().clear_bit()  // PAD counts
         .cpol().clear_bit()
         .cpha().clear_bit()
         .lsbfrst().clear_bit()
         .master().set_bit()
         .sp().bits(0)  // motorola
         .comm().bits(0b01)  // simplex transmitter
         .ioswp().clear_bit()
         .midi().bits(0)  // master inter data idle
         .mssi().bits(0)  // master SS idle
    });
    spi2.cr2.modify(|_, w| unsafe {
        w.tsize().bits(0)
    });
    spi2.cr1.write(|w| w.spe().set_bit());
    spi2.cr1.modify(|r, w| unsafe { w.bits(r.bits() | (1 << 9)) });
}

// DAC1
fn spi4_setup(spi4: &stm32::SPI4) {
    spi4.cfg1.modify(|_, w| unsafe {
        w.mbr().bits(0)  // clk/2
         .dsize().bits(16 - 1)
         .fthvl().bits(1 - 1)  // one data
    });
    spi4.cfg2.modify(|_, w| unsafe {
        w.afcntr().set_bit()
         .ssom().set_bit()  // ss deassert between frames during midi
         .ssoe().set_bit()  // ss output enable
         .ssiop().clear_bit()  // ss active low
         .ssm().clear_bit()  // PAD counts
         .cpol().clear_bit()
         .cpha().clear_bit()
         .lsbfrst().clear_bit()
         .master().set_bit()
         .sp().bits(0)  // motorola
         .comm().bits(0b01)  // simplex transmitter
         .ioswp().clear_bit()
         .midi().bits(0)  // master inter data idle
         .mssi().bits(0)  // master SS idle
    });
    spi4.cr2.modify(|_, w| unsafe {
        w.tsize().bits(0)
    });
    spi4.cr1.write(|w| w.spe().set_bit());
    spi4.cr1.modify(|r, w| unsafe { w.bits(r.bits() | (1 << 9)) });
}

fn tim2_setup(tim2: &stm32::TIM2) {
    tim2.psc.write(|w| unsafe { w.psc().bits(200 - 1) });  // from 200 MHz
    tim2.arr.write(|w| unsafe { w.bits(2 - 1) });  // µs
    tim2.dier.write(|w| w.ude().set_bit().uie().set_bit());  // FIXME
    tim2.egr.write(|w| w.ug().set_bit());
    tim2.cr1.modify(|_, w|
        w.dir().clear_bit()  // up
         .cen().set_bit());  // enable
}

fn dma1_setup(dma1: &stm32::DMA1, dmamux1: &stm32::DMAMUX1, ma: usize, pa: usize) {
    // info!("{:#x} {:#x}", pa, unsafe { *(pa as *const u32) });

    dma1.s0cr.modify(|_, w| w.en().clear_bit());
    while dma1.s0cr.read().en().bit_is_set() {}

    dma1.s0par.write(|w| unsafe { w.pa().bits(pa as u32) });
    dma1.s0m0ar.write(|w| unsafe { w.m0a().bits(ma as u32) });
    dma1.s0ndtr.write(|w| unsafe { w.ndt().bits(1) });
    dmamux1.dmamux1_c0cr.modify(|_, w| unsafe { w.dmareq_id().bits(22) });  // tim2_up
    dma1.s0cr.modify(|_, w| unsafe {
        w.pl().bits(0b11)  // very high
         .circ().set_bit()  // reload ndtr
         .msize().bits(0b10)  // 32
         .minc().clear_bit()
         .mburst().bits(0b00)
         .psize().bits(0b10)  // 32
         .pinc().clear_bit()
         .pburst().bits(0b00)
         .dbm().clear_bit()
         .dir().bits(0b01)  // memory_to_peripheral
         .pfctrl().clear_bit()  // dma is FC
    });
    dma1.s0fcr.modify(|_, w| w.dmdis().clear_bit());
    dma1.s0cr.modify(|_, w| w.en().set_bit());
}

static SPIP: Mutex<RefCell<Option<(
    stm32::SPI1, stm32::SPI2, stm32::SPI4, stm32::SPI5)>>> =
    Mutex::new(RefCell::new(None));

#[link_section = ".sram1"]
static mut DAT: u32 = (1 << 9) | (1 << 0);

#[entry]
fn main() -> ! {
    let mut cp = CorePeripherals::take().unwrap();
    let dp = Peripherals::take().unwrap();

    let rcc = dp.RCC;
    rcc_reset(&rcc);

    init_log();
    // info!("Version {} {}", build_info::PKG_VERSION, build_info::GIT_VERSION.unwrap());
    // info!("Built on {}", build_info::BUILT_TIME_UTC);
    // info!("{} {}", build_info::RUSTC_VERSION, build_info::TARGET);

    pwr_setup(&dp.PWR);
    rcc_pll_setup(&rcc, &dp.FLASH);
    rcc.apb4enr.modify(|_, w| w.syscfgen().set_bit());
    io_compensation_setup(&dp.SYSCFG);

    cp.SCB.enable_icache();
    cp.SCB.enable_dcache(&mut cp.CPUID);
    cp.DWT.enable_cycle_counter();

    rcc.ahb4enr.modify(|_, w|
        w.gpioaen().set_bit()
         .gpioben().set_bit()
         .gpioden().set_bit()
         .gpioeen().set_bit()
         .gpiofen().set_bit()
         .gpiogen().set_bit()
    );
    gpio_setup(&dp.GPIOA, &dp.GPIOB, &dp.GPIOD, &dp.GPIOE, &dp.GPIOF, &dp.GPIOG);

    rcc.apb1lenr.modify(|_, w| w.spi2en().set_bit());
    let spi2 = dp.SPI2;
    spi2_setup(&spi2);

    rcc.apb2enr.modify(|_, w| w.spi4en().set_bit());
    let spi4 = dp.SPI4;
    spi4_setup(&spi4);

    rcc.apb2enr.modify(|_, w| w.spi1en().set_bit());
    let spi1 = dp.SPI1;
    spi1_setup(&spi1);
    spi1.ier.write(|w| w.eotie().set_bit());

    rcc.apb2enr.modify(|_, w| w.spi5en().set_bit());
    let spi5 = dp.SPI5;
    spi5_setup(&spi5);
    // spi5.ier.write(|w| w.eotie().set_bit());

    rcc.ahb2enr.modify(|_, w| w.sram1en().set_bit());
    rcc.ahb1enr.modify(|_, w| w.dma1en().set_bit());
    unsafe { DAT = (1 << 9) | (1 << 0) };  // init SRAM1 rodata can't load with sram1 disabled
    cortex_m::asm::dsb();
    let dat_addr = unsafe { &DAT as *const _ } as usize;
    cp.SCB.clean_dcache_by_address(dat_addr, 4);

    // TODO: also SPI4/ADC0
    dma1_setup(&dp.DMA1, &dp.DMAMUX1, dat_addr,
               &spi1.cr1 as *const _ as usize);

    rcc.apb1lenr.modify(|_, w| w.tim2en().set_bit());
    tim2_setup(&dp.TIM2);

    unsafe {
        let t = 2e-6*2.;
        IIR_CH[0].set_pi(1., 0., 0.).expect("bad coefficients");
        IIR_CH[0].set_x_offset(0.*SCALE);

        IIR_CH[1].set_pi(-0.1, -10.*t, 0.).expect("bad coefficients");
        IIR_CH[1].set_x_offset(0.1*SCALE);
    }

    cortex_m::interrupt::free(|cs| {
        cp.NVIC.enable(stm32::Interrupt::SPI1);
        cp.NVIC.enable(stm32::Interrupt::TIM2);  // FIXME
        SPIP.borrow(cs).replace(Some((spi1, spi2, spi4, spi5)));
    });

    loop {
        for _ in 0..1000000 { cortex_m::asm::wfi(); }
        let (x0, y0, x1, y1) = unsafe {
            (IIR_STATE[0][0], IIR_STATE[0][2], IIR_STATE[1][0], IIR_STATE[1][2]) };
        info!("x0={} y0={} x1={} y1={}", x0, y0, x1, y1);
    }
}

#[interrupt]
fn TIM2() {  // FIXME
    let dp = unsafe { Peripherals::steal() };
    dp.TIM2.sr.write(|w| w.uif().clear_bit());  // rc_w0
    dp.SPI1.cr1.write(|w| unsafe { w.bits(0x201) });  // ADC0
    dp.SPI5.cr1.write(|w| unsafe { w.bits(0x201) });  // ADC1
}

const SCALE: f32 = ((1 << 15) - 1) as f32;
static mut IIR_STATE: [IIRState; 2] = [[0.; 5]; 2];
static mut IIR_CH: [IIR; 2] = [
    IIR{ ba: [0., 0., 0., 0., 0.], y_offset: 0.,
         y_min: -SCALE, y_max: SCALE }; 2];

#[interrupt]
fn SPI1() {
    #[cfg(feature = "bkpt")]
    cortex_m::asm::bkpt();
    cortex_m::interrupt::free(|cs| {
        let spip = SPIP.borrow(cs).borrow();
        let (spi1, spi2, spi4, spi5) = spip.as_ref().unwrap();

        let sr = spi1.sr.read();
        if sr.eot().bit_is_set() {
           spi1.ifcr.write(|w| w.eotc().set_bit());
        }
        if sr.rxp().bit_is_set() {
            let rxdr1 = &spi1.rxdr as *const _ as *const u16;
            let a = unsafe { ptr::read_volatile(rxdr1) };
            let x0 = a as i16 as f32;
            let y0 = unsafe { IIR_CH[0].update(&mut IIR_STATE[0], x0) };
            let d = y0 as i16 as u16 ^ 0x8000;

            let txdr2 = &spi2.txdr as *const _ as *mut u16;
            unsafe { ptr::write_volatile(txdr2, d) };
        }

        let sr = spi5.sr.read();
        if sr.eot().bit_is_set() {
           spi5.ifcr.write(|w| w.eotc().set_bit());
        }
        if sr.rxp().bit_is_set() {
            let rxdr1 = &spi5.rxdr as *const _ as *const u16;
            let a = unsafe { ptr::read_volatile(rxdr1) };
            let x0 = a as i16 as f32;
            let y0 = unsafe { IIR_CH[1].update(&mut IIR_STATE[1], x0) };
            let d = y0 as i16 as u16 ^ 0x8000;

            let txdr2 = &spi4.txdr as *const _ as *mut u16;
            unsafe { ptr::write_volatile(txdr2, d) };
        }
    });
    #[cfg(feature = "bkpt")]
    cortex_m::asm::bkpt();
}

#[exception]
fn HardFault(ef: &cortex_m_rt::ExceptionFrame) -> ! {
    panic!("HardFault at {:#?}", ef);
}

#[exception]
fn DefaultHandler(irqn: i16) {
    panic!("Unhandled exception (IRQn = {})", irqn);
}
