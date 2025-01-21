use strict;
use warnings;
use diagnostics;

# USAGE: find . -iname "*.rs" -execdir sh -c 'perl ~/openvmm/rules/fixup_read_from_prefix_split.pl {} | sponge {}' \;

# Check if filename is provided
if (@ARGV != 1) {
    die "Usage: $0 filename\n";
}

my $filename = $ARGV[0];

# Open the file for reading
open my $fh, '<', $filename or die "Could not open '$filename' for reading: $!\n";

# Read the entire file content into a single string
my $content = do { local $/; <$fh> };
close $fh;

# Apply regex substitutions
# 127,128c127,128
# <                             let (vendor_guid, path_data) = Guid::read_from_prefix_split(path_data)
# <                                 .ok_or(Error::InvalidLength)?;
# ---
# >                             let (vendor_guid, path_data) = Guid::read_from_prefix(path_data)
# >                                 .map_err(|_| Error::InvalidLength)?; // TODO: zerocopy: map_err
# 248c248
$content =~ s/read_from_prefix_split\(([^)]+)\)\s*\.ok_or\(([^)]+)\)([;]?)([?]?)([;)]?)/read_from_prefix($1).map_err(|_| $2)$3$4$5 \/\/ todo: zerocopy: map_err/g;

# -        let (header, buf) = EFI_SIGNATURE_DATA::read_from_prefix_split(self.buf)
# -            .expect("buf size validated in `new`");
# ---
# +        let (header, buf) = EFI_SIGNATURE_DATA::read_from_prefix(self.buf)
# +            .expect("buf size validated in `new`"); // TODO: zerocopy: expect
$content =~ s/read_from_prefix_split\(([^)]+)\)\s*\.expect\(([^)]+)\)([;]?)/read_from_prefix($1).expect($2)$3 \/\/ TODO: zerocopy: expect/g;

# -                                boot::EfiExpandedAcpiDevice::read_from_prefix_split(path_data)
# -                                    .unwrap();
# +                                boot::EfiExpandedAcpiDevice::read_from_prefix(path_data)
# +                                    .unwrap(); // TODO: zerocopy: unwrap
$content =~ s/read_from_prefix_split\(([^)]+)\)\s*\.unwrap\(\)([;]?)/read_from_prefix($1).unwrap()$2 \/\/ TODO: zerocopy: unwrap/g;

# Adjust use statements:
# * rename AsBytes -> IntoBytes
# * add KnownLayout, Immutable, FromZeros
# * remove references to FromZeroes
#
# We can't just replace FromZeroes -> FromZeros: each module that calls T::from_zeroes now needs to use `zerocopy::FromZeros`, which
# was not true for `zerocopy::FromZeroes`.
$content =~ s/use zerocopy::AsBytes;/use zerocopy::IntoBytes; use zerocopy::Immutable; use zerocopy::KnownLayout; use zerocopy::FromZeros;/g;
$content =~ s/use zerocopy::FromZeroes;[\r\n]//g;

# Fixup derive(...FromZeroes...) (remove FromZeroes; it is no longer needed in zerocopy 0.8)
# But, if the *only* derive is FromZeroes, replace that with the standard zerocopy 0.8 derives.
$content =~ s/derive\(FromZeroes\)/derive(FromZeros, Immutable, KnownLayout)/g;
$content =~ s/(derive\(.*)\s*FromZeroes[,]?\s*([^)]*)/$1$2/g;

# Now rename FromZeroes -> FromZeros (for example, for cases where smallvec![FromZeroes::new_zeroed(); len])
$content =~ s/FromZeroes/FromZeros/g;

# Fixup derive(...AsBytes...) (rename AsBytes -> IntoBytes, add Immutable, KnownLayout)
$content =~ s/(derive\(.*)\s*AsBytes([,]?)\s*([^)]*)/$1IntoBytes, Immutable, KnownLayout$2$3/g;

# Fixup type bounds to include Immutable and KnownLayout if IntoBytes or FromBytes are present
#$content =~ s/([^\:][\:][^\:][^,>)]*AsBytes[^,>)]*|[^\:][\:][^\:][^,>)]*FromBytes([^,>)]*))/add_immutable_knownlayout_bounds($1)/egx;
$content =~ s/([^\:][\:][^\:][^,>){;]*AsBytes[^,>){]*|[^\:][\:][^\:][^,>){;]*FromBytes[^,>){]*)([>,){])/add_immutable_knownlayout_bounds($1, $2)/egx;

sub add_immutable_knownlayout_bounds {
    my ($bounds, $suffix) = @_;
    $bounds .= ' + Immutable' unless $bounds =~ /Immutable/;
    $bounds .= ' + KnownLayout' unless $bounds =~ /KnownLayout/;
    return "$bounds$suffix";
}

$content =~ s/([^\:]\:[^\),>]*)AsBytes([^\),>]*)/$1IntoBytes$2/g;

# Add .0 to read_from_prefix calls, e.g.
# let hdr = GpaRange::read_from_prefix(self.buf[0].as_bytes()).unwrap();
$content =~ s/(read_from_prefix\(.*\.unwrap\(\))[^\.][^0](;?)/$1.0$2 \/\/ todo: zerocopy: use-rest-of-range/g;

# let fh = EFI_FFS_FILE_HEADER::read_from_prefix(&image[image_offset as usize..])?;
$content =~ s/(read_from_prefix\([^)]*\))(\?)(;?)/$1.ok()$2.0$3 \/\/ todo: zerocopy: use-rest-of-range, option-to-error/g;

$content =~ s/(read_from_prefix\([^)]*\)\s*)\.ok_or\(([^)]*\))([?]?)([;)]?)/$1.map_err(|_| $2$3.0$4 \/\/ todo: zerocopy: map_err/g;

# < *XsaveHeader::mut_from_prefix(&mut data[XSAVE_LEGACY_LEN..]).unwrap() = XsaveHeader {
# > *XsaveHeader::mut_from_prefix(&mut data[XSAVE_LEGACY_LEN..]).unwrap().0 = XsaveHeader { // todo: zerocopy: mut-from-prefix unwrap
$content =~ s/((mut_from_prefix|read_from_prefix)\((?:[^)(]|\((?:[^)(]|\((?:[^)(]|\([^)(]*\))*\))*\))*\)[\n\s]*.unwrap\(\))[^\.][^0]([^\n;]*)(;?)/$1.0$3$4 \/\/ todo: zerocopy: from-prefix ($2): use-rest-of-range/g;

$content =~ s/(ref_from_prefix\(.*\.unwrap\(\))(;?)/$1.0$2 \/\/ todo: zerocopy: ref-from-prefix: use-rest-of-range/g;


$content =~ s/([\w_]+)\.into_ref\(\)/Ref::into_ref($1)/g;

# Print the updated content
print $content;
