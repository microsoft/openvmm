// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides an in-memory implementation of [`NvramStorage`] that doesn't
//! automatically persist to disk.

use crate::NextVariable;
use crate::NvramStorage;
use crate::NvramStorageError;
use crate::EFI_TIME;
use guid::Guid;
use std::collections::BTreeMap;
use std::fmt::Display;
use ucs2::Ucs2LeSlice;
use ucs2::Ucs2LeVec;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct VariableKey {
    vendor: Guid,
    name: Ucs2LeVec,
}

impl Display for VariableKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.vendor, self.name)
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
struct Variable {
    data: Vec<u8>,
    timestamp: EFI_TIME,
    #[cfg_attr(feature = "inspect", inspect(hex))]
    attr: u32,
}

/// An in-memory implementation of [`NvramStorage`].
#[derive(Debug)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
pub struct InMemoryNvram {
    #[cfg_attr(feature = "inspect", inspect(iter_by_key))]
    nvram: BTreeMap<VariableKey, Variable>,
}

pub struct VariableEntry<'a> {
    pub vendor: Guid,
    pub name: &'a Ucs2LeSlice,
    pub data: &'a [u8],
    pub timestamp: EFI_TIME,
    pub attr: u32,
}

impl InMemoryNvram {
    pub fn new() -> Self {
        Self {
            nvram: Default::default(),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = VariableEntry<'_>> {
        self.nvram.iter().map(|(k, v)| VariableEntry {
            vendor: k.vendor,
            name: k.name.as_ref(),
            data: v.data.as_slice(),
            timestamp: v.timestamp,
            attr: v.attr,
        })
    }

    pub fn clear(&mut self) {
        self.nvram.clear()
    }
}

#[async_trait::async_trait]
impl NvramStorage for InMemoryNvram {
    async fn get_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
    ) -> Result<Option<(u32, Vec<u8>, EFI_TIME)>, NvramStorageError> {
        Ok(self
            .nvram
            .get(&VariableKey {
                vendor,
                name: name.to_ucs2_le_vec(),
            })
            .map(|v| (v.attr, v.data.clone(), v.timestamp)))
    }

    async fn set_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
        attr: u32,
        data: Vec<u8>,
        timestamp: EFI_TIME,
    ) -> Result<(), NvramStorageError> {
        self.nvram.insert(
            VariableKey {
                vendor,
                name: name.to_ucs2_le_vec(),
            },
            Variable {
                data,
                timestamp,
                attr,
            },
        );
        Ok(())
    }

    async fn append_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
        data: Vec<u8>,
        timestamp: EFI_TIME,
    ) -> Result<bool, NvramStorageError> {
        match self.nvram.get_mut(&VariableKey {
            vendor,
            name: name.to_ucs2_le_vec(),
        }) {
            Some(val) => {
                val.data.extend_from_slice(&data);
                val.timestamp = timestamp;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn remove_variable(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
    ) -> Result<bool, NvramStorageError> {
        Ok(self
            .nvram
            .remove(&VariableKey {
                vendor,
                name: name.to_ucs2_le_vec(),
            })
            .is_some())
    }

    async fn next_variable(
        &mut self,
        name_vendor: Option<(&Ucs2LeSlice, Guid)>,
    ) -> Result<NextVariable, NvramStorageError> {
        let key = &name_vendor.map(|(name, vendor)| VariableKey {
            vendor,
            name: name.to_ucs2_le_vec(),
        });

        if let Some(key) = key {
            let mut range = self.nvram.range(key..);
            if let Some((found_key, _)) = range.next() {
                if found_key == key {
                    Ok(match range.next() {
                        Some(v) => NextVariable::Exists {
                            name: v.0.name.clone(),
                            vendor: v.0.vendor,
                            attr: v.1.attr,
                        },
                        None => NextVariable::EndOfList,
                    })
                } else {
                    Ok(NextVariable::InvalidKey)
                }
            } else {
                Ok(NextVariable::EndOfList)
            }
        } else {
            Ok(match self.nvram.iter().next() {
                Some(v) => NextVariable::Exists {
                    name: v.0.name.clone(),
                    vendor: v.0.vendor,
                    attr: v.1.attr,
                },
                None => NextVariable::EndOfList,
            })
        }
    }
}

/// A collection of test-implementation helpers that operate on a generic
/// implementation of [`NvramStorage`]
pub mod impl_agnostic_tests {
    use crate::NextVariable;
    use crate::NvramStorage;
    use crate::EFI_TIME;
    use guid::Guid;
    use ucs2::Ucs2LeSlice;
    use wchar::wchz;
    use zerocopy::FromZeros;
    use zerocopy::IntoBytes;

    pub async fn test_single_variable(nvram: &mut dyn NvramStorage) {
        let vendor = Guid::new_random();
        let name = Ucs2LeSlice::from_slice_with_nul(wchz!(u16, "var1").as_bytes()).unwrap();
        let attr = 0x1234;
        let data = vec![0x1, 0x2, 0x3, 0x4, 0x5];
        let data1 = vec![0xa, 0xb, 0xc];
        let timestamp = EFI_TIME::new_zeroed();

        nvram
            .set_variable(name, vendor, attr, data.clone(), timestamp)
            .await
            .unwrap();

        let (result_attr, result_data, result_timestamp) =
            nvram.get_variable(name, vendor).await.unwrap().unwrap();
        assert_eq!(result_attr, attr);
        assert_eq!(result_data, data);
        assert_eq!(result_timestamp, timestamp);

        let result = nvram.next_variable(Some((name, vendor))).await.unwrap();
        assert!(matches!(result, NextVariable::EndOfList));

        // set existing variable with new data
        nvram
            .set_variable(name, vendor, attr, data1.clone(), timestamp)
            .await
            .unwrap();

        let (result_attr, result_data, result_timestamp) =
            nvram.get_variable(name, vendor).await.unwrap().unwrap();
        assert_eq!(result_attr, attr);
        assert_eq!(result_data, data1);
        assert_eq!(result_timestamp, timestamp);

        nvram.remove_variable(name, vendor).await.unwrap();

        // try to get removed variable
        let result = nvram.get_variable(name, vendor).await.unwrap();
        assert!(result.is_none());
    }

    pub async fn test_next(nvram: &mut dyn NvramStorage) {
        let vendor1 = Guid::new_random();
        let name1 = Ucs2LeSlice::from_slice_with_nul(wchz!(u16, "var1").as_bytes()).unwrap();
        let vendor2 = Guid::new_random();
        let name2 = Ucs2LeSlice::from_slice_with_nul(wchz!(u16, "var2").as_bytes()).unwrap();
        let vendor3 = Guid::new_random();
        let name3 = Ucs2LeSlice::from_slice_with_nul(wchz!(u16, "var3").as_bytes()).unwrap();
        let attr = 0x1234;
        let data = vec![0x1, 0x2, 0x3, 0x4, 0x5];
        let timestamp = EFI_TIME::new_zeroed();

        nvram
            .set_variable(name1, vendor1, attr, data.clone(), timestamp)
            .await
            .unwrap();
        nvram
            .set_variable(name2, vendor2, attr, data.clone(), timestamp)
            .await
            .unwrap();
        nvram
            .set_variable(name3, vendor3, attr, data, timestamp)
            .await
            .unwrap();

        let mut expected = {
            let mut s = std::collections::BTreeSet::new();

            s.insert(NextVariable::Exists {
                name: name1.to_owned(),
                vendor: vendor1,
                attr,
            });
            s.insert(NextVariable::Exists {
                name: name2.to_owned(),
                vendor: vendor2,
                attr,
            });
            s.insert(NextVariable::Exists {
                name: name3.to_owned(),
                vendor: vendor3,
                attr,
            });

            s
        };

        let mut owned_key;
        let mut key = None;
        loop {
            let var = nvram.next_variable(key).await.unwrap();
            match &var {
                NextVariable::InvalidKey => panic!(),
                NextVariable::EndOfList => break,
                NextVariable::Exists {
                    name,
                    vendor,
                    attr: _,
                } => owned_key = Some((name.clone(), *vendor)),
            };

            key = owned_key
                .as_ref()
                .map(|(name, vendor)| (name.as_ref(), *vendor));

            let removed = expected.remove(&var);
            assert!(removed);
        }

        assert!(expected.is_empty());

        // check to make sure calls to next_variable are idempotent

        let var1 = nvram.next_variable(None).await.unwrap();
        let var2 = nvram.next_variable(None).await.unwrap();
        assert_eq!(var1, var2);

        let key = match nvram.next_variable(None).await.unwrap() {
            NextVariable::Exists {
                name,
                vendor,
                attr: _,
            } => Some((name, vendor)),
            _ => panic!(),
        };
        let key = key.as_ref().map(|(name, vendor)| (name.as_ref(), *vendor));

        let var1 = nvram.next_variable(key).await.unwrap();
        let var2 = nvram.next_variable(key).await.unwrap();
        assert_eq!(var1, var2);
    }

    pub async fn test_multiple_variable(nvram: &mut dyn NvramStorage) {
        let vendor1 = Guid::new_random();
        let name1 = Ucs2LeSlice::from_slice_with_nul(wchz!(u16, "var1").as_bytes()).unwrap();
        let vendor2 = Guid::new_random();
        let name2 = Ucs2LeSlice::from_slice_with_nul(wchz!(u16, "var2").as_bytes()).unwrap();
        let vendor3 = Guid::new_random();
        let name3 = Ucs2LeSlice::from_slice_with_nul(wchz!(u16, "var3").as_bytes()).unwrap();
        let attr = 0x1234;
        let data = vec![0x1, 0x2, 0x3, 0x4, 0x5];
        let timestamp = EFI_TIME::new_zeroed();

        // add all variables to nvram
        nvram
            .set_variable(name1, vendor1, attr, data.clone(), timestamp)
            .await
            .unwrap();
        nvram
            .set_variable(name2, vendor2, attr, data.clone(), timestamp)
            .await
            .unwrap();
        nvram
            .set_variable(name3, vendor3, attr, data.clone(), timestamp)
            .await
            .unwrap();

        let (result_attr, result_data, result_timestamp) =
            nvram.get_variable(name1, vendor1).await.unwrap().unwrap();
        assert_eq!(result_attr, attr);
        assert_eq!(result_data, data);
        assert_eq!(result_timestamp, timestamp);

        let (result_attr, result_data, result_timestamp) =
            nvram.get_variable(name2, vendor2).await.unwrap().unwrap();
        assert_eq!(result_attr, attr);
        assert_eq!(result_data, data);
        assert_eq!(result_timestamp, timestamp);

        let (result_attr, result_data, result_timestamp) =
            nvram.get_variable(name3, vendor3).await.unwrap().unwrap();
        assert_eq!(result_attr, attr);
        assert_eq!(result_data, data);
        assert_eq!(result_timestamp, timestamp);

        // throw an append in there for good measure
        let appended = nvram
            .append_variable(name1, vendor1, vec![6, 7, 8], timestamp)
            .await
            .unwrap();
        assert!(appended);

        let (result_attr, result_data, result_timestamp) =
            nvram.get_variable(name1, vendor1).await.unwrap().unwrap();
        assert_eq!(result_attr, attr);
        assert_eq!(result_data, (1..=8).collect::<Vec<u8>>());
        assert_eq!(result_timestamp, timestamp);
    }
}

#[cfg(test)]
mod tests {
    use super::impl_agnostic_tests;
    use super::*;
    use pal_async::async_test;

    #[async_test]
    async fn nvram_trait_single_variable() {
        let mut nvram = InMemoryNvram::new();
        impl_agnostic_tests::test_single_variable(&mut nvram).await;
    }

    #[async_test]
    async fn nvram_trait_next() {
        let mut nvram = InMemoryNvram::new();
        impl_agnostic_tests::test_next(&mut nvram).await;
    }

    #[async_test]
    async fn nvram_trait_multiple_variable() {
        let mut nvram = InMemoryNvram::new();
        impl_agnostic_tests::test_multiple_variable(&mut nvram).await;
    }
}
