// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Implements DirectoryInformation for a type.
///
/// Takes inputs in the form `type, attr, tag;`, where `attr` is the struct field from which
/// `file_attributes` is returned, and `tag` is the field from which `reparse_tag` is returned.
macro_rules! impl_directory_information {
    ($($type:ty, $attr:ident, $tag:ident;)*) => {$(
        impl DirectoryInformation for $type {
            fn file_id(&self) -> i64 {
                self.FileId
            }

            fn file_name(&self) -> Result<UnicodeStringRef<'_>, lx::Error> {
                // SAFETY: A properly constructed struct will contain the name in a buffer at the end.
                let name_slice = unsafe {
                    std::slice::from_raw_parts(
                        self.FileName.as_ptr(),
                        self.FileNameLength as usize / size_of::<u16>(),
                    )
                };
                UnicodeStringRef::new(name_slice).ok_or(lx::Error::EINVAL)
            }

            fn file_attributes(&self) -> u32 {
                self.$attr
            }

            fn reparse_tag(&self) -> u32 {
                self.$tag
            }
        })*
    };
}
pub(crate) use impl_directory_information;

/// A macro to implement FileInformationClass.
/// Takes any number of inputs of the format `FILE_X_INFORMATION = FileXInformation;`
macro_rules! file_information_classes {
    ($($class:ty = $info:expr;)*) => {
        $(impl FileInformationClass for $class {
            fn file_information_class(&self) -> FileSystem::FILE_INFORMATION_CLASS {
                $info
            }

            fn as_ptr_len(&self) -> (*const u8, usize) {
                (ptr::from_ref::<Self>(self).cast::<u8>(), size_of::<Self>())
            }

            fn as_ptr_len_mut(&mut self) -> (*mut u8, usize) {
                (ptr::from_mut::<Self>(self).cast::<u8>(), size_of::<Self>())
            }
        })*
    };
}
pub(crate) use file_information_classes;
