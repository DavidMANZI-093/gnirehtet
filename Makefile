# Gnirehtet build - Android app + Rust relay.
#
# Requirements:
#   - JDK 17+ (javac, jar, java, keytool)
#   - Android SDK with platforms/android-$(API) and build-tools/$(BT)
#     (set ANDROID_HOME or ANDROID_SDK_ROOT; see local.env)
#   - zip (APK packaging)
#   - base64 (only when RELEASE_KEYSTORE_BASE64 is used)
#   - curl (app-test JUnit jars)
#   - adb (only for app-install/app-tunnel/app-start/app-stop)
#   - cargo (only for the relay targets)
#
# Layout:
#   app/src/main/AndroidManifest.xml
#   app/src/main/java/          the app's Java sources
#   app/src/main/res/           resources (compiled by aapt2)
#   app/src/test/java/          JUnit tests (see `make app-test`)
#   app/tools/                  app build helpers (test stubs)
#   app/build/                  app output (gitignored)
#   relay-rust/                 Rust relay (built with cargo)
#
# App pipeline per variant:
#   aapt2 compile -> aapt2 link (+ R.java) -> javac (+ BuildConfig.java)
#   -> jar -> d8 -> append classes.dex -> zipalign -> apksigner

# --- tool locations ---------------------------------------------------------

SDK         ?= $(or $(ANDROID_HOME),$(ANDROID_SDK_ROOT))
BT          ?= 36.0.0
API         ?= 36
PLATFORM    ?= $(SDK)/platforms/android-$(API)
BUILD_TOOLS ?= $(SDK)/build-tools/$(BT)
ANDROID_JAR ?= $(PLATFORM)/android.jar
AAPT2       ?= $(BUILD_TOOLS)/aapt2
D8          ?= $(BUILD_TOOLS)/d8
ZIPALIGN    ?= $(BUILD_TOOLS)/zipalign
APKSIGNER   ?= $(BUILD_TOOLS)/apksigner

JAVAC       ?= javac
JAR         ?= jar
JAVA        ?= java
KEYTOOL     ?= keytool
ZIP         ?= zip
CURL        ?= curl
ADB         ?= adb
CARGO       ?= cargo

SDKMANAGER  ?= $(if $(wildcard $(SDK)/cmdline-tools/latest/bin/sdkmanager),$(SDK)/cmdline-tools/latest/bin/sdkmanager,sdkmanager)

# --- project settings -------------------------------------------------------

APP          := app
RELAY        := relay-rust
BUILD        := $(APP)/build

PKG          := d3fault.gnirehtet
PKG_PATH     := d3fault/gnirehtet
ACTIVITY     := $(PKG)/.GnirehtetActivity
START_ACTION := $(PKG).START
STOP_ACTION  := $(PKG).STOP

VERSION_CODE ?= 9
VERSION_NAME ?= 2.5.1
MIN_SDK      ?= 21
PORT         ?= 31416

# Java language level for javac. 8 keeps the code close to the original app
JAVA_RELEASE ?= 8
JAVACFLAGS   := --release $(JAVA_RELEASE) -Xlint:-options -g -encoding UTF-8
# `make app-lint` turns every javac warning on (except the noisy "options" one).
LINTFLAGS    := --release $(JAVA_RELEASE) -Xlint:all,-options -Xmaxwarns 1000 -g -encoding UTF-8

# Debug signing. `make app-keystore` generates it on first use.
DEBUG_KEYSTORE  ?= $(HOME)/.android/debug.keystore
DEBUG_KS_PASS   ?= android
DEBUG_KEY_ALIAS ?= androiddebugkey

# Release signing. In CI, RELEASE_KEYSTORE_BASE64 is decoded to
# RELEASE_KEYSTORE; locally, point RELEASE_KEYSTORE at an existing keystore.
RELEASE_KEYSTORE        ?= $(BUILD)/release.p12
RELEASE_KEYSTORE_BASE64 ?=
RELEASE_KEY_PASS        ?=
RELEASE_KEY_ALIAS       ?=

export DEBUG_KS_PASS RELEASE_KEYSTORE_BASE64 RELEASE_KEY_PASS

# --- output layout ----------------------------------------------------------

GEN          := $(BUILD)/gen
CLASSES      := $(BUILD)/classes
DEX          := $(BUILD)/dex
RES_ZIP      := $(BUILD)/res.zip
BASE_APK     := $(BUILD)/base.apk
PACKED_APK   := $(BUILD)/gnirehtet-unsigned.apk
ALIGNED_APK  := $(BUILD)/gnirehtet-aligned.apk
APK_DEBUG    := $(BUILD)/gnirehtet-debug.apk
APK_RELEASE  := $(BUILD)/gnirehtet-release.apk
BUILD_CONFIG := $(GEN)/$(PKG_PATH)/BuildConfig.java
R_JAVA       := $(GEN)/$(PKG_PATH)/R.java

MAIN_SOURCES := $(shell [ -d $(APP)/src/main/java ] && find $(APP)/src/main/java -name '*.java' || true)
TEST_SOURCES := $(shell [ -d $(APP)/src/test/java ] && find $(APP)/src/test/java -name '*.java' || true)
TEST_CLASSES := $(subst /,.,$(patsubst $(APP)/src/test/java/%.java,%,$(TEST_SOURCES)))
STUB_SOURCES := $(shell [ -d $(APP)/tools/test-stubs ] && find $(APP)/tools/test-stubs -name '*.java' || true)

TEST_DEPS    := $(BUILD)/test-deps
LINT_CLASSES := $(BUILD)/lint-classes
JUNIT_JAR    := $(TEST_DEPS)/junit-4.13.2.jar
HAMCREST_JAR := $(TEST_DEPS)/hamcrest-core-1.3.jar

.DEFAULT_GOAL := help
.PHONY: help all \
	app app-debug app-release app-install app-uninstall app-tunnel app-start app-stop \
	app-lint app-test app-gen app-info app-check app-keystore app-clean \
	relay relay-release relay-test relay-install relay-clean relay-check check \
	release-keystore release-check

# --- build macros -----------------------------------------------------------

# $(1) extra aapt2 link flags (e.g. --debug-mode)
define link-apk
	$(AAPT2) link -o $(BASE_APK) -I $(ANDROID_JAR) \
		--manifest $(APP)/src/main/AndroidManifest.xml \
		--java $(GEN) --min-sdk-version $(MIN_SDK) --target-sdk-version $(API) \
		--version-code $(VERSION_CODE) --version-name $(VERSION_NAME) $(1) $(RES_ZIP)
endef

# $(1) value of BuildConfig.DEBUG
define write-buildconfig
	printf 'package %s;\n\npublic final class BuildConfig {\n    public static final boolean DEBUG = %s;\n\n    private BuildConfig() {\n    }\n}\n' \
		'$(PKG)' '$(1)' > $(BUILD_CONFIG)
endef

# $(1) --debug-mode or empty, $(2) BuildConfig.DEBUG, $(3) extra d8 flags,
# $(4) output APK, $(5) keystore, $(6) store password env var, $(7) key alias
define run-build
	@mkdir -p $(GEN) $(CLASSES) $(DEX)
	$(AAPT2) compile --dir $(APP)/src/main/res -o $(RES_ZIP)
	$(call link-apk,$(1))
	$(call write-buildconfig,$(2))
	$(JAVAC) $(JAVACFLAGS) -cp $(ANDROID_JAR) -d $(CLASSES) \
		$(MAIN_SOURCES) $(R_JAVA) $(BUILD_CONFIG)
	$(JAR) cf $(BUILD)/classes.jar -C $(CLASSES) .
	$(D8) --lib $(ANDROID_JAR) --min-api $(MIN_SDK) $(3) --output $(DEX) $(BUILD)/classes.jar
	cp $(BASE_APK) $(PACKED_APK)
	$(ZIP) -q -j $(PACKED_APK) $(DEX)/classes.dex
	$(ZIPALIGN) -f -p 4 $(PACKED_APK) $(ALIGNED_APK)
	rm -f $(4)
	$(APKSIGNER) sign --ks "$(5)" --ks-pass env:$(6) \
		--ks-key-alias "$(7)" --out $(4) $(ALIGNED_APK)
endef

# --- targets ----------------------------------------------------------------

help:
	@echo "Gnirehtet (Gradle-free build)"
	@echo
	@echo "  App (Android):"
	@echo "  make app           build $(APK_DEBUG)"
	@echo "  make app-release   build $(APK_RELEASE) (release keystore required)"
	@echo "  make app-install   adb install -r the debug APK"
	@echo "  make app-tunnel    adb reverse localabstract:gnirehtet -> tcp:$(PORT)"
	@echo "  make app-start     send the START intent over adb"
	@echo "  make app-stop      send the STOP intent over adb"
	@echo "  make app-lint      compile with javac -Xlint:all and show warnings"
	@echo "  make app-test      run JUnit tests from $(APP)/src/test/java"
	@echo "  make app-info      show the debug APK manifest summary"
	@echo "  make app-clean     remove $(BUILD)/"
	@echo
	@echo "  Relay (Rust):"
	@echo "  make relay         cargo build"
	@echo "  make relay-release cargo build --release"
	@echo "  make relay-test    cargo test"
	@echo "  make relay-check   cargo check"
	@echo "  make relay-install cargo install --path $(RELAY)"
	@echo "  make relay-clean   cargo clean"
	@echo
	@echo "  SDK=$(if $(SDK),$(SDK),(unset: export ANDROID_HOME))"
	@echo "  BT=$(BT)  API=$(API)"
	@echo "  VERSION_CODE=$(VERSION_CODE)  VERSION_NAME=$(VERSION_NAME)  MIN_SDK=$(MIN_SDK)"
	@echo
	@echo "  Release signing: RELEASE_KEYSTORE_BASE64 (or RELEASE_KEYSTORE)"
	@echo "                   RELEASE_KEY_PASS  RELEASE_KEY_ALIAS"

all: app relay

# --- app --------------------------------------------------------------------

app: app-debug

app-debug: app-check app-keystore
	$(call run-build,--debug-mode,true,,$(APK_DEBUG),$(DEBUG_KEYSTORE),DEBUG_KS_PASS,$(DEBUG_KEY_ALIAS))

app-release: app-check release-check
	$(call run-build,,false,--release,$(APK_RELEASE),$(RELEASE_KEYSTORE),RELEASE_KEY_PASS,$(RELEASE_KEY_ALIAS))

app-check:
	@if [ -z "$(SDK)" ]; then \
		echo "SDK is not set."; \
		echo "  export ANDROID_HOME=\"$$HOME/android-sdk\"   # or ANDROID_SDK_ROOT"; \
		echo "  or: make app SDK=/path/to/android-sdk"; \
		exit 1; \
	fi; \
	missing=; \
	for f in "$(ANDROID_JAR)" "$(AAPT2)" "$(D8)" "$(ZIPALIGN)" "$(APKSIGNER)"; do \
		[ -e "$$f" ] || { echo "missing: $$f"; missing=1; }; \
	done; \
	for c in "$(JAVAC)" "$(JAR)" "$(JAVA)" "$(KEYTOOL)" "$(ZIP)"; do \
		command -v "$$c" >/dev/null || { echo "missing in PATH: $$c"; missing=1; }; \
	done; \
	[ -z "$$missing" ] || { \
		echo; \
		echo "Installed build-tools: $(notdir $(wildcard $(SDK)/build-tools/*))"; \
		echo "Install the missing SDK pieces with:"; \
		echo "  $(SDKMANAGER) --sdk_root=$(SDK) 'platforms;android-$(API)' 'build-tools;$(BT)'"; \
		exit 1; \
	}

app-keystore:
	@test -f "$(DEBUG_KEYSTORE)" || { \
		echo "generating $(DEBUG_KEYSTORE)"; \
		mkdir -p "$(dir $(DEBUG_KEYSTORE))"; \
		$(KEYTOOL) -genkeypair -keystore "$(DEBUG_KEYSTORE)" -storepass "$(DEBUG_KS_PASS)" -keypass "$(DEBUG_KS_PASS)" \
			-alias "$(DEBUG_KEY_ALIAS)" -keyalg RSA -keysize 2048 -validity 10000 \
			-dname "CN=Android Debug,O=Android,C=US"; \
	}

release-keystore:
	@test "$(abspath $(RELEASE_KEYSTORE))" != "$(abspath $(DEBUG_KEYSTORE))" || { echo "RELEASE_KEYSTORE points at the debug keystore"; exit 1; }
	@if [ -n "$$RELEASE_KEYSTORE_BASE64" ]; then \
		command -v base64 >/dev/null || { echo "missing in PATH: base64"; exit 1; }; \
		umask 077; \
		mkdir -p "$(dir $(RELEASE_KEYSTORE))"; \
		printf '%s' "$$RELEASE_KEYSTORE_BASE64" | base64 -d > "$(RELEASE_KEYSTORE)"; \
	fi
	@test -f "$(RELEASE_KEYSTORE)" || { echo "missing release keystore: $(RELEASE_KEYSTORE)"; \
		echo "  set RELEASE_KEYSTORE_BASE64 (CI) or RELEASE_KEYSTORE (local)"; exit 1; }

release-check: release-keystore
	@test -n "$$RELEASE_KEY_PASS" || { echo "RELEASE_KEY_PASS is not set"; exit 1; }
	@test -n "$(RELEASE_KEY_ALIAS)" || { echo "RELEASE_KEY_ALIAS is not set"; exit 1; }

app-install: app-debug
	$(ADB) install -r $(APK_DEBUG)

app-uninstall:
	$(ADB) uninstall $(PKG)

app-tunnel:
	$(ADB) reverse localabstract:gnirehtet tcp:$(PORT)

app-start:
	$(ADB) shell am start -a $(START_ACTION) -n $(ACTIVITY)

app-stop:
	$(ADB) shell am start -a $(STOP_ACTION) -n $(ACTIVITY)

app-info: app-debug
	$(AAPT2) dump badging $(APK_DEBUG) | head -12

# --- app tests --------------------------------------------------------------

# Enough of the build to compile test sources: R.java + BuildConfig + android.jar.
app-gen: app-check
	@mkdir -p $(GEN)
	$(AAPT2) compile --dir $(APP)/src/main/res -o $(RES_ZIP)
	$(call link-apk,)
	$(call write-buildconfig,true)

$(JUNIT_JAR):
	@mkdir -p $(TEST_DEPS)
	$(CURL) -fsSL -o $@ https://repo1.maven.org/maven2/junit/junit/4.13.2/junit-4.13.2.jar

$(HAMCREST_JAR):
	@mkdir -p $(TEST_DEPS)
	$(CURL) -fsSL -o $@ https://repo1.maven.org/maven2/org/hamcrest/hamcrest-core/1.3/hamcrest-core-1.3.jar

ifeq ($(TEST_SOURCES),)
app-test:
	@echo "no tests in $(APP)/src/test/java - nothing to do"
else
app-test: app-gen $(JUNIT_JAR) $(HAMCREST_JAR)
	@mkdir -p $(BUILD)/stub-classes $(BUILD)/test-classes
	$(JAVAC) $(JAVACFLAGS) -d $(BUILD)/stub-classes $(STUB_SOURCES)
	$(JAVAC) $(JAVACFLAGS) -cp $(ANDROID_JAR):$(JUNIT_JAR) -d $(BUILD)/test-classes \
		$(MAIN_SOURCES) $(TEST_SOURCES) $(R_JAVA) $(BUILD_CONFIG)
	$(JAVA) -cp "$(BUILD)/stub-classes:$(BUILD)/test-classes:$(JUNIT_JAR):$(HAMCREST_JAR):$(ANDROID_JAR)" \
		org.junit.runner.JUnitCore $(TEST_CLASSES)
endif

# --- app lint ---------------------------------------------------------------

# Compile the app sources with all javac lint warnings enabled. Reports
# warning details; findings do not fail the build (-Werror could enforce).
app-lint: app-gen
	@mkdir -p $(LINT_CLASSES)
	$(JAVAC) $(LINTFLAGS) -cp $(ANDROID_JAR) -d $(LINT_CLASSES) \
		$(MAIN_SOURCES) $(R_JAVA) $(BUILD_CONFIG)

app-clean:
	rm -rf $(BUILD)

# --- relay (Rust) -----------------------------------------------------------

check:
	@test -f "$(RELAY)/Cargo.toml" || { echo "missing: $(RELAY)/Cargo.toml"; exit 1; }
	@command -v "$(CARGO)" >/dev/null || { echo "not found: $(CARGO)"; exit 1; }

relay: check
	$(CARGO) build --manifest-path $(RELAY)/Cargo.toml

relay-release: check
	$(CARGO) build --release --manifest-path $(RELAY)/Cargo.toml

relay-test: check
	$(CARGO) test --manifest-path $(RELAY)/Cargo.toml

relay-check: check
	$(CARGO) check --manifest-path $(RELAY)/Cargo.toml

relay-install: check
	$(CARGO) install --path $(RELAY)

relay-clean: check
	$(CARGO) clean --manifest-path $(RELAY)/Cargo.toml
