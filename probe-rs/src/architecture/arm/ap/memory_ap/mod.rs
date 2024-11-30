//! Memory access port

pub(crate) mod mock;
pub mod registers;

mod amba_ahb3;
mod amba_apb2_apb3;
mod amba_apb4_apb5;

mod amba_ahb5;
mod amba_ahb5_hprot;

mod amba_axi3_axi4;
mod amba_axi5;

pub use registers::DataSize;
use registers::{AddressIncrement, BaseAddrFormat, BASE, BASE2, DRW, TAR, TAR2};

use super::{AccessPortError, AccessPortType, ApAccess, ApRegAccess};
use crate::architecture::arm::{ArmError, DapAccess, FullyQualifiedApAddress, Register};

/// Implements all default registers of a memory AP to the given type.
///
/// Invoke in the form `attached_regs_to_mem_ap!(mod_name => ApName)` where:
/// - `mod_name` is a module name in which the impl an the required use will be expanded to.
/// - `ApName` a type name that must be available in the current scope to which the registers will
///   be attached.
#[macro_export]
macro_rules! attached_regs_to_mem_ap {
    ($mod_name:ident => $name:ident) => {
        mod $mod_name {
            use super::$name;
            use $crate::architecture::arm::ap::{
                memory_ap::registers::{
                    BASE, BASE2, BD0, BD1, BD2, BD3, CFG, CSW, DRW, MBT, TAR, TAR2,
                },
                ApRegAccess,
            };
            impl ApRegAccess<CFG> for $name {}
            impl ApRegAccess<CSW> for $name {}
            impl ApRegAccess<BASE> for $name {}
            impl ApRegAccess<BASE2> for $name {}
            impl ApRegAccess<TAR> for $name {}
            impl ApRegAccess<TAR2> for $name {}
            impl ApRegAccess<BD2> for $name {}
            impl ApRegAccess<BD3> for $name {}
            impl ApRegAccess<DRW> for $name {}
            impl ApRegAccess<MBT> for $name {}
            impl ApRegAccess<BD1> for $name {}
            impl ApRegAccess<BD0> for $name {}
        }
    };
}

pub trait MemoryApType:
    ApRegAccess<BASE> + ApRegAccess<BASE2> + ApRegAccess<TAR> + ApRegAccess<TAR2> + ApRegAccess<DRW>
{
    /// This Memory AP’s specific CSW type.
    type CSW: Register;

    fn has_large_address_extension(&self) -> bool;
    fn has_large_data_extension(&self) -> bool;
    fn supports_only_32bit_data_size(&self) -> bool;

    /// Attempts to set the requested data size.
    ///
    /// The operation may fail if the requested data size is not supported by the Memory Access
    /// Port.
    async fn try_set_datasize<I: ApAccess>(
        &mut self,
        interface: &mut I,
        data_size: DataSize,
    ) -> Result<(), ArmError>;

    /// The current generic CSW (missing the memory AP specific fields).
    async fn generic_status<I: ApAccess>(
        &mut self,
        interface: &mut I,
    ) -> Result<registers::CSW, ArmError> {
        self.status(interface)
            .await?
            .into()
            .try_into()
            .map_err(ArmError::RegisterParse)
    }

    /// The current CSW with the memory AP specific fields.
    async fn status<I: ApAccess>(&mut self, interface: &mut I) -> Result<Self::CSW, ArmError>;

    /// The base address of this AP which is used to then access all relative control registers.
    async fn base_address<I: ApAccess>(&self, interface: &mut I) -> Result<u64, ArmError> {
        let base_register: BASE = interface.read_ap_register(self).await?;

        let mut base_address = if BaseAddrFormat::ADIv5 == base_register.Format {
            let base2: BASE2 = interface.read_ap_register(self).await?;

            u64::from(base2.BASEADDR) << 32
        } else {
            0
        };
        base_address |= u64::from(base_register.BASEADDR << 12);

        Ok(base_address)
    }

    async fn set_target_address<I: ApAccess>(
        &mut self,
        interface: &mut I,
        address: u64,
    ) -> Result<(), ArmError> {
        let address_lower = address as u32;
        let address_upper = (address >> 32) as u32;

        if self.has_large_address_extension() {
            let tar = TAR2 {
                address: address_upper,
            };
            interface.write_ap_register(self, tar).await?;
        } else if address_upper != 0 {
            return Err(ArmError::OutOfBounds);
        }

        let tar = TAR {
            address: address_lower,
        };
        interface.write_ap_register(self, tar).await?;

        Ok(())
    }

    /// Read multiple 32 bit values from the DRW register on the given AP.
    async fn read_data<I: ApAccess>(
        &mut self,
        interface: &mut I,
        values: &mut [u32],
    ) -> Result<(), ArmError> {
        match values {
            // If transferring only 1 word, use non-repeated register access, because it might be
            // faster depending on the probe.
            [value] => interface.read_ap_register(self).await.map(|drw: DRW| {
                *value = drw.data;
            }),
            _ => {
                interface
                    .read_ap_register_repeated::<_, DRW>(self, values)
                    .await
            }
        }
        .map_err(AccessPortError::register_read_error::<DRW, _>)
        .map_err(|err| ArmError::from_access_port(err, self.ap_address()))
    }

    /// Write multiple 32 bit values to the DRW register on the given AP.
    async fn write_data<I: ApAccess>(
        &mut self,
        interface: &mut I,
        values: &[u32],
    ) -> Result<(), ArmError> {
        match values {
            // If transferring only 1 word, use non-repeated register access, because it might be
            // faster depending on the probe.
            &[data] => interface.write_ap_register(self, DRW { data }).await,
            _ => {
                interface
                    .write_ap_register_repeated::<_, DRW>(self, values)
                    .await
            }
        }
        .map_err(AccessPortError::register_write_error::<DRW, _>)
        .map_err(|e| ArmError::from_access_port(e, self.ap_address()))
    }
}

macro_rules! memory_aps {
    ($($variant:ident => $type:path),*) => {
        #[derive(Debug)]
        pub enum MemoryAp {
            $($variant($type)),*
        }

        $(impl From<$type> for MemoryAp {
            fn from(value: $type) -> Self {
                Self::$variant(value)
            }
        })*

        impl MemoryAp {
            pub async fn new<I: DapAccess>(
                interface: &mut I,
                address: &FullyQualifiedApAddress,
            ) -> Result<Self, ArmError> {
                use crate::architecture::arm::{ap::IDR, Register};
                let idr: IDR = interface
                    .read_raw_ap_register(address, IDR::ADDRESS).await?
                    .try_into()?;
                tracing::debug!("reading IDR: {:x?}", idr);
                use crate::architecture::arm::ap::ApType;
                Ok(match idr.TYPE {
                    ApType::JtagComAp => return Err(ArmError::WrongApType),
                    $(ApType::$variant => <$type>::new(interface, address.clone()).await?.into(),)*
                })
            }
        }
    }
}

memory_aps! {
    AmbaAhb3 => amba_ahb3::AmbaAhb3,
    AmbaAhb5 => amba_ahb5::AmbaAhb5,
    AmbaAhb5Hprot => amba_ahb5_hprot::AmbaAhb5Hprot,
    AmbaApb2Apb3 => amba_apb2_apb3::AmbaApb2Apb3,
    AmbaApb4Apb5 => amba_apb4_apb5::AmbaApb4Apb5,
    AmbaAxi3Axi4 => amba_axi3_axi4::AmbaAxi3Axi4,
    AmbaAxi5 => amba_axi5::AmbaAxi5
}

impl ApRegAccess<super::IDR> for MemoryAp {}
attached_regs_to_mem_ap!(memory_ap_regs => MemoryAp);

macro_rules! mem_ap_forward {
    ($me:ident, $name:ident($($arg:ident),*)) => { mem_ap_forward!($me, $name($($arg),*); ) };
    ($me:ident, async $name:ident($($arg:ident),*)) => { mem_ap_forward!($me, $name($($arg),*); .await) };
    ($me:ident, $name:ident($($arg:ident),*); $($a:tt)*) => {
        match $me {
            MemoryAp::AmbaApb2Apb3(ap) => ap.$name($($arg),*)$($a)*,
            MemoryAp::AmbaApb4Apb5(ap) => ap.$name($($arg),*)$($a)*,
            MemoryAp::AmbaAhb3(m) => m.$name($($arg),*)$($a)*,
            MemoryAp::AmbaAhb5(m) => m.$name($($arg),*)$($a)*,
            MemoryAp::AmbaAhb5Hprot(m) => m.$name($($arg),*)$($a)*,
            MemoryAp::AmbaAxi3Axi4(m) => m.$name($($arg),*)$($a)*,
            MemoryAp::AmbaAxi5(m) => m.$name($($arg),*)$($a)*,
        }
    }
}

impl AccessPortType for MemoryAp {
    fn ap_address(&self) -> &crate::architecture::arm::FullyQualifiedApAddress {
        mem_ap_forward!(self, ap_address())
    }
}

impl MemoryApType for MemoryAp {
    type CSW = registers::CSW;

    fn has_large_address_extension(&self) -> bool {
        mem_ap_forward!(self, has_large_address_extension())
    }

    fn has_large_data_extension(&self) -> bool {
        mem_ap_forward!(self, has_large_data_extension())
    }

    fn supports_only_32bit_data_size(&self) -> bool {
        mem_ap_forward!(self, supports_only_32bit_data_size())
    }

    async fn try_set_datasize<I: ApAccess>(
        &mut self,
        interface: &mut I,
        data_size: DataSize,
    ) -> Result<(), ArmError> {
        mem_ap_forward!(self, async try_set_datasize(interface, data_size))
    }

    async fn status<I: ApAccess>(&mut self, interface: &mut I) -> Result<Self::CSW, ArmError> {
        mem_ap_forward!(self, async generic_status(interface))
    }
}
