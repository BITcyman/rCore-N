use crate::future::GetWakerFuture;
use crate::trace::{SERIAL_INTR_ENTER, SERIAL_INTR_EXIT};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::future::Future;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering::Relaxed;
use core::task::{Context, Poll, Waker};
use core::{convert::Infallible, pin::Pin, sync::atomic::AtomicBool};
use embedded_hal::serial::{Read, Write};
use heapless::spsc;
#[cfg(feature = "board_lrv")]
use lrv_pac::uart;
#[cfg(feature = "board_qemu")]
use qemu_pac::uart;
pub use serial_config::*;
use spin::Mutex;

pub const DEFAULT_TX_BUFFER_SIZE: usize = 1000;
pub const DEFAULT_RX_BUFFER_SIZE: usize = 1000;

#[cfg(feature = "board_qemu")]
mod serial_config {
    pub use uart8250::{uart::LSR, InterruptType, MmioUart8250};
    pub type SerialHardware = MmioUart8250<'static>;
    pub const FIFO_DEPTH: usize = 16;
    pub const SERIAL_NUM: usize = 4;
    pub const SERIAL_BASE_ADDRESS: usize = 0x1000_2000;
    pub const SERIAL_ADDRESS_STRIDE: usize = 0x1000;
    pub fn irq_to_serial_id(irq: u16) -> usize {
        match irq {
            12 => 0,
            13 => 1,
            14 => 2,
            15 => 3,
            _ => 0,
        }
    }
}

#[cfg(feature = "board_lrv")]
mod serial_config {
    pub use uart_xilinx::uart_16550::{uart::LSR, InterruptType, MmioUartAxi16550};
    pub type SerialHardware = MmioUartAxi16550<'static>;
    pub const FIFO_DEPTH: usize = 16;
    pub const SERIAL_NUM: usize = 4;
    pub const SERIAL_BASE_ADDRESS: usize = 0x6000_1000;
    pub const SERIAL_ADDRESS_STRIDE: usize = 0x1000;
    pub fn irq_to_serial_id(irq: u16) -> usize {
        match irq {
            4 => 0,
            5 => 1,
            6 => 2,
            7 => 3,
            _ => 0,
        }
    }
}

pub fn get_base_addr_from_irq(irq: u16) -> usize {
    SERIAL_BASE_ADDRESS + irq_to_serial_id(irq) * SERIAL_ADDRESS_STRIDE
}

pub struct BufferedSerial {
    // pub hardware: SerialHardware,
    base_address: usize,

    pub rx_buffer: VecDeque<u8>,
    pub tx_buffer: VecDeque<u8>,
    pub rx_count: usize,
    pub tx_count: usize,
    pub intr_count: usize,
    pub rx_intr_count: usize,
    pub tx_intr_count: usize,
    pub tx_fifo_count: usize,
    rx_intr_enabled: bool,
    tx_intr_enabled: bool,
}

impl BufferedSerial {
    pub fn new(base_address: usize) -> Self {
        BufferedSerial {
            // hardware: SerialHardware::new(base_address),
            base_address,
            rx_buffer: VecDeque::with_capacity(DEFAULT_RX_BUFFER_SIZE),
            tx_buffer: VecDeque::with_capacity(DEFAULT_TX_BUFFER_SIZE),
            rx_count: 0,
            tx_count: 0,
            intr_count: 0,
            rx_intr_count: 0,
            tx_intr_count: 0,
            tx_fifo_count: 0,
            rx_intr_enabled: false,
            tx_intr_enabled: false,
        }
    }

    fn hardware(&self) -> &uart::RegisterBlock {
        unsafe { &*(self.base_address as *const _) }
    }

    fn set_divisor(&self, clock: usize, baud_rate: usize) {
        let block = self.hardware();
        let divisor = clock / (16 * baud_rate);
        block.lcr.write(|w| w.dlab().set_bit());
        #[cfg(feature = "board_lrv")]
        {
            block
                .dll()
                .write(|w| unsafe { w.bits((divisor & 0b1111_1111) as u32) });
            block
                .dlh()
                .write(|w| unsafe { w.bits(((divisor >> 8) & 0b1111_1111) as u32) });
        }
        #[cfg(feature = "board_qemu")]
        {
            block
                .dll()
                .write(|w| unsafe { w.bits((divisor & 0b1111_1111) as u8) });
            block
                .dlh()
                .write(|w| unsafe { w.bits(((divisor >> 8) & 0b1111_1111) as u8) });
        }

        block.lcr.write(|w| w.dlab().clear_bit());
    }

    pub(super) fn enable_rdai(&mut self) {
        self.hardware().ier().modify(|_, w| w.erbfi().enable());
        // println!("enable rdai");
        self.rx_intr_enabled = true;
    }

    fn disable_rdai(&mut self) {
        self.hardware().ier().modify(|_, w| w.erbfi().disable());
        // println!("disable rdai");
        self.rx_intr_enabled = false;
    }

    pub(super) fn enable_threi(&mut self) {
        self.hardware().ier().modify(|_, w| w.etbei().enable());
        self.tx_intr_enabled = true;
    }

    fn disable_threi(&mut self) {
        self.hardware().ier().modify(|_, w| w.etbei().disable());
        self.tx_intr_enabled = false;
    }

    fn try_recv(&self) -> Option<u8> {
        let block = self.hardware();
        if block.lsr.read().dr().bit_is_set() {
            Some(block.rbr().read().bits() as _)
        } else {
            None
        }
    }

    fn send(&self, ch: u8) {
        let block = self.hardware();
        block.thr().write(|w| w.thr().variant(ch));
    }

    pub fn hardware_init(&mut self, baud_rate: usize) {
        let block = self.hardware();
        let _unused = block.msr.read().bits();
        let _unused = block.lsr.read().bits();
        block.lcr.reset();
        // No modem control
        block.mcr.reset();
        block.ier().reset();
        block.fcr().reset();

        // Enable DLAB and Set divisor
        self.set_divisor(100_000_000, baud_rate);
        // Disable DLAB and set word length 8 bits, no parity, 1 stop bit
        block
            .lcr
            .modify(|_, w| w.dls().eight().pen().disabled().stop().one());
        // Enable FIFO
        block.fcr().write(|w| {
            w.fifoe()
                .clear_bit()
                .rfifor()
                .set_bit()
                .xfifor()
                .set_bit()
                .rt()
                .one_character()
        });

        // Enable received_data_available_interrupt
        self.enable_rdai();
    }

    #[cfg(any(feature = "board_qemu", feature = "board_lrv"))]
    pub fn interrupt_handler(&mut self) {
        // println!("[SERIAL] Interrupt!");

        use uart::iir::IID_A;

        use crate::trace::push_trace;
        while let Some(int_type) = self.hardware().iir().read().iid().variant() {
            if int_type == IID_A::NO_INTERRUPT_PENDING {
                break;
            }
            let intr_id: usize = int_type as u8 as _;
            push_trace(SERIAL_INTR_ENTER + intr_id);
            self.intr_count += 1;
            match int_type {
                IID_A::RECEIVED_DATA_AVAILABLE | IID_A::CHARACTER_TIMEOUT => {
                    // println!("[SERIAL] Received data available");
                    self.rx_intr_count += 1;
                    while let Some(ch) = self.try_recv() {
                        if self.rx_buffer.len() < DEFAULT_TX_BUFFER_SIZE {
                            self.rx_buffer.push_back(ch);
                            self.rx_count += 1;
                        } else {
                            // println!("[USER UART] Serial rx buffer overflow!");
                            self.disable_rdai();
                            break;
                        }
                    }
                }
                IID_A::THR_EMPTY => {
                    // println!("[SERIAL] Transmitter Holding Register Empty");
                    self.tx_intr_count += 1;
                    for _ in 0..FIFO_DEPTH {
                        if let Some(ch) = self.tx_buffer.pop_front() {
                            self.send(ch);
                            self.tx_count += 1;
                        } else {
                            self.disable_threi();
                            break;
                        }
                    }
                }
                IID_A::MODEM_STATUS => {
                    let block = self.hardware();
                    println!(
                        "[USER SERIAL] MSR: {:#x}, LSR: {:#x}, IER: {:#x}",
                        block.msr.read().bits(),
                        block.lsr.read().bits(),
                        block.ier().read().bits()
                    );
                }
                _ => {
                    println!("[USER SERIAL] {:?} not supported!", int_type);
                }
            }
            push_trace(SERIAL_INTR_EXIT + intr_id);
        }
    }
}

impl Write<u8> for BufferedSerial {
    type Error = Infallible;

    // #[cfg(any(feature = "board_qemu", feature = "board_lrv"))]
    // fn try_write(&mut self, word: u8) -> nb::Result<(), Self::Error> {
    //     let serial = &mut self.hardware;
    //     if self.tx_buffer.len() < DEFAULT_TX_BUFFER_SIZE {
    //         self.tx_buffer.push_back(word);
    //         if !self.tx_intr_enabled {
    //             serial.enable_transmitter_holding_register_empty_interrupt();
    //             self.tx_intr_enabled = true;
    //         }
    //     } else {
    //         // println!("[USER SERIAL] Tx buffer overflow!");
    //         return Err(nb::Error::WouldBlock);
    //     }
    //     Ok(())
    // }

    #[cfg(any(feature = "board_qemu", feature = "board_lrv"))]
    fn try_write(&mut self, word: u8) -> nb::Result<(), Self::Error> {
        if self.tx_buffer.len() < DEFAULT_TX_BUFFER_SIZE {
            self.tx_buffer.push_back(word);
            if !self.tx_intr_enabled {
                self.enable_threi();
            }
        } else {
            // println!("[USER SERIAL] Tx buffer overflow!");
            return Err(nb::Error::WouldBlock);
        }
        Ok(())
    }

    fn try_flush(&mut self) -> nb::Result<(), Self::Error> {
        todo!()
    }
}

impl Read<u8> for BufferedSerial {
    type Error = Infallible;

    fn try_read(&mut self) -> nb::Result<u8, Self::Error> {
        if let Some(ch) = self.rx_buffer.pop_front() {
            Ok(ch)
        } else {
            if !self.rx_intr_enabled {
                self.enable_rdai();
            }
            Err(nb::Error::WouldBlock)
        }
    }
}

impl Drop for BufferedSerial {
    fn drop(&mut self) {
        let block = self.hardware();
        block.ier().reset();
        let _unused = block.msr.read().bits();
        let _unused = block.lsr.read().bits();
        // reset Rx & Tx FIFO, disable FIFO
        block
            .fcr()
            .write(|w| w.fifoe().clear_bit().rfifor().set_bit().xfifor().set_bit());
    }
}

pub struct PollingSerial {
    base_address: usize,
    pub rx_count: usize,
    pub tx_count: usize,
    pub tx_fifo_count: usize,
}

impl PollingSerial {
    pub fn new(base_address: usize) -> Self {
        PollingSerial {
            base_address,
            rx_count: 0,
            tx_count: 0,
            tx_fifo_count: 0,
        }
    }

    fn hardware(&self) -> &uart::RegisterBlock {
        unsafe { &*(self.base_address as *const _) }
    }

    fn set_divisor(&self, clock: usize, baud_rate: usize) {
        let block = self.hardware();
        let divisor = clock / (16 * baud_rate);
        block.lcr.write(|w| w.dlab().set_bit());
        #[cfg(feature = "board_lrv")]
        {
            block
                .dll()
                .write(|w| unsafe { w.bits((divisor & 0b1111_1111) as u32) });
            block
                .dlh()
                .write(|w| unsafe { w.bits(((divisor >> 8) & 0b1111_1111) as u32) });
        }
        #[cfg(feature = "board_qemu")]
        {
            block
                .dll()
                .write(|w| unsafe { w.bits((divisor & 0b1111_1111) as u8) });
            block
                .dlh()
                .write(|w| unsafe { w.bits(((divisor >> 8) & 0b1111_1111) as u8) });
        }

        block.lcr.write(|w| w.dlab().clear_bit());
    }

    fn try_recv(&self) -> Option<u8> {
        let block = self.hardware();
        if block.lsr.read().dr().bit_is_set() {
            Some(block.rbr().read().bits() as _)
        } else {
            None
        }
    }

    fn send(&self, ch: u8) {
        let block = self.hardware();
        block.thr().write(|w| w.thr().variant(ch));
    }

    pub fn hardware_init(&mut self, baud_rate: usize) {
        let block = self.hardware();
        let _unused = block.msr.read().bits();
        let _unused = block.lsr.read().bits();
        block.lcr.reset();
        // No modem control
        block.mcr.reset();
        block.ier().reset();
        block.fcr().reset();

        // Enable DLAB and Set divisor
        self.set_divisor(100_000_000, baud_rate);
        // Disable DLAB and set word length 8 bits, no parity, 1 stop bit
        block
            .lcr
            .modify(|_, w| w.dls().eight().pen().disabled().stop().one());
        // Enable FIFO
        block.fcr().write(|w| {
            w.fifoe()
                .set_bit()
                .rfifor()
                .set_bit()
                .xfifor()
                .set_bit()
                .rt()
                .two_less_than_full()
        });
    }

    pub fn interrupt_handler(&mut self) {}
}

impl Write<u8> for PollingSerial {
    type Error = Infallible;

    #[cfg(any(feature = "board_qemu", feature = "board_lrv"))]
    fn try_write(&mut self, word: u8) -> nb::Result<(), Self::Error> {
        while self.tx_fifo_count >= FIFO_DEPTH {
            if self.hardware().lsr.read().thre().bit_is_set() {
                self.tx_fifo_count = 0;
            }
        }
        self.send(word);
        self.tx_count += 1;
        self.tx_fifo_count += 1;
        Ok(())
    }

    fn try_flush(&mut self) -> nb::Result<(), Self::Error> {
        todo!()
    }
}

impl Read<u8> for PollingSerial {
    type Error = Infallible;

    #[cfg(any(feature = "board_qemu", feature = "board_lrv"))]
    fn try_read(&mut self) -> nb::Result<u8, Self::Error> {
        if let Some(ch) = self.try_recv() {
            self.rx_count += 1;
            Ok(ch)
        } else {
            Err(nb::Error::WouldBlock)
        }
    }
}

impl Drop for PollingSerial {
    fn drop(&mut self) {
        let block = self.hardware();
        block.ier().reset();
        let _unused = block.msr.read().bits();
        let _unused = block.lsr.read().bits();
        // reset Rx & Tx FIFO, disable FIFO
        block
            .fcr()
            .write(|w| w.fifoe().clear_bit().rfifor().set_bit().xfifor().set_bit());
    }
}

type RxProducer = spsc::Producer<'static, u8, DEFAULT_RX_BUFFER_SIZE>;
type RxConsumer = spsc::Consumer<'static, u8, DEFAULT_RX_BUFFER_SIZE>;
type TxProducer = spsc::Producer<'static, u8, DEFAULT_TX_BUFFER_SIZE>;
type TxConsumer = spsc::Consumer<'static, u8, DEFAULT_TX_BUFFER_SIZE>;

pub struct AsyncSerial {
    base_address: usize,
    rx_pro: Mutex<RxProducer>,
    rx_con: Mutex<RxConsumer>,
    tx_pro: Mutex<TxProducer>,
    tx_con: Mutex<TxConsumer>,
    pub rx_count: AtomicUsize,
    pub tx_count: AtomicUsize,
    pub intr_count: AtomicUsize,
    pub rx_intr_count: AtomicUsize,
    pub tx_intr_count: AtomicUsize,
    pub(super) rx_intr_enabled: AtomicBool,
    pub(super) tx_intr_enabled: AtomicBool,
    read_waker: Mutex<Option<Waker>>,
    write_waker: Mutex<Option<Waker>>,
}

impl AsyncSerial {
    pub fn new(
        base_address: usize,
        rx_pro: RxProducer,
        rx_con: RxConsumer,
        tx_pro: TxProducer,
        tx_con: TxConsumer,
    ) -> Self {
        AsyncSerial {
            base_address,
            rx_pro: Mutex::new(rx_pro),
            rx_con: Mutex::new(rx_con),
            tx_pro: Mutex::new(tx_pro),
            tx_con: Mutex::new(tx_con),
            rx_count: AtomicUsize::new(0),
            tx_count: AtomicUsize::new(0),
            intr_count: AtomicUsize::new(0),
            rx_intr_count: AtomicUsize::new(0),
            tx_intr_count: AtomicUsize::new(0),
            rx_intr_enabled: AtomicBool::new(false),
            tx_intr_enabled: AtomicBool::new(false),
            read_waker: Mutex::new(None),
            write_waker: Mutex::new(None),
        }
    }

    fn hardware(&self) -> &uart::RegisterBlock {
        unsafe { &*(self.base_address as *const _) }
    }

    fn set_divisor(&self, clock: usize, baud_rate: usize) {
        let block = self.hardware();
        let divisor = clock / (16 * baud_rate);
        block.lcr.write(|w| w.dlab().set_bit());
        #[cfg(feature = "board_lrv")]
        {
            block
                .dll()
                .write(|w| unsafe { w.bits((divisor & 0b1111_1111) as u32) });
            block
                .dlh()
                .write(|w| unsafe { w.bits(((divisor >> 8) & 0b1111_1111) as u32) });
        }
        #[cfg(feature = "board_qemu")]
        {
            block
                .dll()
                .write(|w| unsafe { w.bits((divisor & 0b1111_1111) as u8) });
            block
                .dlh()
                .write(|w| unsafe { w.bits(((divisor >> 8) & 0b1111_1111) as u8) });
        }

        block.lcr.write(|w| w.dlab().clear_bit());
    }

    pub(super) fn enable_rdai(&self) {
        self.hardware().ier().modify(|_, w| w.erbfi().set_bit());
        self.rx_intr_enabled.store(true, Relaxed);
    }

    fn disable_rdai(&self) {
        self.hardware().ier().modify(|_, w| w.erbfi().clear_bit());
        self.rx_intr_enabled.store(false, Relaxed);
    }

    pub(super) fn enable_threi(&self) {
        self.hardware().ier().modify(|_, w| w.etbei().set_bit());
        self.tx_intr_enabled.store(true, Relaxed);
    }

    fn disable_threi(&self) {
        self.hardware().ier().modify(|_, w| w.etbei().clear_bit());
        self.tx_intr_enabled.store(false, Relaxed);
    }

    fn try_recv(&self) -> Option<u8> {
        let block = self.hardware();
        if block.lsr.read().dr().bit_is_set() {
            Some(block.rbr().read().bits() as _)
        } else {
            None
        }
    }

    fn send(&self, ch: u8) {
        let block = self.hardware();
        block.thr().write(|w| w.thr().variant(ch));
    }

    pub(super) fn try_read(&self) -> Option<u8> {
        if let Some(mut rx_lock) = self.rx_con.try_lock() {
            rx_lock.dequeue()
        } else {
            None
        }
    }

    pub(super) fn try_write(&self, ch: u8) -> Result<(), u8> {
        if let Some(mut tx_lock) = self.tx_pro.try_lock() {
            tx_lock.enqueue(ch)
        } else {
            Err(ch)
        }
    }

    pub fn hardware_init(&self, baud_rate: usize) {
        let block = self.hardware();
        let _unused = block.msr.read().bits();
        let _unused = block.lsr.read().bits();
        block.lcr.reset();
        // No modem control
        block.mcr.reset();
        block.ier().reset();
        block.fcr().reset();

        // Enable DLAB and Set divisor
        self.set_divisor(100_000_000, baud_rate);
        // Disable DLAB and set word length 8 bits, no parity, 1 stop bit
        block
            .lcr
            .modify(|_, w| w.dls().eight().pen().disabled().stop().one());
        // Enable FIFO
        block.fcr().write(|w| {
            w.fifoe()
                .set_bit()
                .rfifor()
                .set_bit()
                .xfifor()
                .set_bit()
                .rt()
                .half_full()
        });

        // Enable received_data_available_interrupt
        self.enable_rdai();
    }

    #[cfg(any(feature = "board_qemu", feature = "board_lrv"))]
    pub fn interrupt_handler(&self) {
        // println!("[SERIAL] Interrupt!");

        use uart::iir::IID_A;

        use crate::trace::push_trace;
        let block = self.hardware();
        while let Some(int_type) = block.iir().read().iid().variant() {
            if int_type == IID_A::NO_INTERRUPT_PENDING {
                break;
            }
            let intr_id: usize = int_type as u8 as _;
            push_trace(SERIAL_INTR_ENTER + intr_id);
            self.intr_count.fetch_add(1, Relaxed);
            match int_type {
                IID_A::RECEIVED_DATA_AVAILABLE | IID_A::CHARACTER_TIMEOUT => {
                    // println!("[SERIAL] Received data available");
                    self.rx_intr_count.fetch_add(1, Relaxed);
                    let mut rx_count = 0;
                    let mut pro = self.rx_pro.lock();
                    while let Some(ch) = self.try_recv() {
                        if let Ok(()) = pro.enqueue(ch) {
                            rx_count += 1;
                        } else {
                            // println!("[USER UART] Serial rx buffer overflow!");
                            self.disable_rdai();
                            break;
                        }
                    }
                    self.rx_count.fetch_add(rx_count, Relaxed);
                    if let Some(mut waker) = self.read_waker.try_lock() {
                        if waker.is_some() {
                            // println!("reader wake");
                            waker.take().unwrap().wake();
                        } else {
                            // println!("no reader waker");
                        }
                    } else {
                        println!("cannot lock reader waker");
                    }
                }
                IID_A::THR_EMPTY => {
                    // println!("[SERIAL] Transmitter Holding Register Empty");
                    self.tx_intr_count.fetch_add(1, Relaxed);
                    let mut tx_count = 0;
                    let mut con = self.tx_con.lock();
                    for _ in 0..FIFO_DEPTH {
                        if let Some(ch) = con.dequeue() {
                            self.send(ch);
                            tx_count += 1;
                        } else {
                            self.disable_threi();
                            break;
                        }
                    }
                    self.tx_count.fetch_add(tx_count, Relaxed);
                    if let Some(mut waker) = self.write_waker.try_lock() {
                        if waker.is_some() {
                            // println!("writer wake");
                            waker.take().unwrap().wake();
                        } else {
                            // println!("no writer waker");
                        }
                    } else {
                        println!("cannot lock writer waker");
                    }
                }
                IID_A::MODEM_STATUS => {
                    println!(
                        "[USER SERIAL] MSR: {:#x}, LSR: {:#x}, IER: {:#x}",
                        block.msr.read().bits(),
                        block.lsr.read().bits(),
                        block.ier().read().bits()
                    );
                }
                _ => {
                    println!("[USER SERIAL] {:?} not supported!", int_type);
                }
            }
            push_trace(SERIAL_INTR_EXIT + intr_id);
        }
    }

    async fn register_read(&self) {
        let raw_waker = GetWakerFuture.await;
        self.read_waker.lock().replace(raw_waker);
    }

    pub async fn read(self: Arc<Self>, buf: &mut [u8]) {
        let future = SerialReadFuture {
            buf,
            read_len: 0,
            driver: self.clone(),
        };
        self.register_read().await;
        future.await;
    }

    async fn register_write(&self) {
        let raw_waker = GetWakerFuture.await;
        self.write_waker.lock().replace(raw_waker);
    }

    pub async fn write(self: Arc<Self>, buf: &[u8]) {
        let future = SerialWriteFuture {
            buf,
            write_len: 0,
            driver: self.clone(),
        };
        self.register_write().await;
        future.await;
    }
}

impl Drop for AsyncSerial {
    fn drop(&mut self) {
        let block = self.hardware();
        block.ier().reset();
        let _unused = block.msr.read().bits();
        let _unused = block.lsr.read().bits();
        // reset Rx & Tx FIFO, disable FIFO
        block
            .fcr()
            .write(|w| w.fifoe().clear_bit().rfifor().set_bit().xfifor().set_bit());
    }
}

struct SerialReadFuture<'a> {
    buf: &'a mut [u8],
    read_len: usize,
    driver: Arc<AsyncSerial>,
}

impl Future for SerialReadFuture<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        // println!("read poll");
        while let Some(data) = self.driver.try_read() {
            if self.read_len < self.buf.len() {
                let len = self.read_len;
                self.buf[len] = data;
                self.read_len += 1;
            } else {
                // println!("reader poll finished");
                return Poll::Ready(());
            }
        }

        if !self.driver.rx_intr_enabled.load(Relaxed) {
            // println!("read intr enabled");
            self.driver.enable_rdai();
        }
        // println!("read poll pending");
        Poll::Pending
    }
}

struct SerialWriteFuture<'a> {
    buf: &'a [u8],
    write_len: usize,
    driver: Arc<AsyncSerial>,
}

impl Future for SerialWriteFuture<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        // println!("write poll");

        while let Ok(()) = self.driver.try_write(self.buf[self.write_len]) {
            if self.write_len < self.buf.len() - 1 {
                self.write_len += 1;
            } else {
                // println!("writer poll finished");
                return Poll::Ready(());
            }
        }

        if !self.driver.tx_intr_enabled.load(Relaxed) {
            // println!("write intr enabled");
            self.driver.enable_threi();
        }
        Poll::Pending
    }
}
