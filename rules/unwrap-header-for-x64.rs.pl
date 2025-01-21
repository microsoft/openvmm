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

$content =~ s/\.unwrap\(\)(\s*)\.header/.unwrap().0$1.header/gm;

# Print the updated content
print $content;
