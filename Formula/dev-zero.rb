class DevZero < Formula
  desc "CLI and TUI for dev-null.sh"
  homepage "https://github.com/sorafujitani/dev-zero"
  url "https://github.com/sorafujitani/dev-zero.git", tag: "v0.1.0"
  license "MIT"

  depends_on "rust" => :build
  depends_on "openssh"

  def install
    system "cargo", "install", *std_cargo_args
    bin.env_script_all_files(libexec, PATH: "#{Formula["openssh"].opt_bin}:$PATH")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/dz --version")
    assert_match "dev-null.sh", shell_output("#{bin}/dz --help")
  end
end
