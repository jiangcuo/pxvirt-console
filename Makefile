# Makefile for pxvdistream-client
include /usr/share/dpkg/default.mk

BUILD_DIR := build

# 需要复制的文件和目录
COPY_ITEMS := build.rs src debian Cargo.toml

.PHONY: deb clean 

ARCH := $(shell dpkg-architecture -qDEB_HOST_ARCH)
TARGET_ARCH := $(shell dpkg-architecture -qDEB_TARGET_ARCH)

# 清理构建目录和生成的包文件
clean:
	@echo "==> 清理构建目录..."
	rm -rf $(BUILD_DIR)
	rm -f *.deb *.changes *.buildinfo
	@echo "==> 清理完成"


deb: 
	@rm -rf $(BUILD_DIR)
	@mkdir -p $(BUILD_DIR)
	@for item in $(COPY_ITEMS); do \
		cp -r $$item $(BUILD_DIR)/; \
	done
	@if git rev-parse --short=8 HEAD >/dev/null 2>&1; then \
		git rev-parse --short=8 HEAD > $(BUILD_DIR)/.git-revision; \
	fi

	@if [ "$(ARCH)" != "$(TARGET_ARCH)" ]; then \
		cd $(BUILD_DIR) && dpkg-buildpackage -us -uc -b -a$(ARCH) -Pcross; \
	else \
		cd $(BUILD_DIR) && dpkg-buildpackage -us -uc -b -a$(ARCH); \
	fi
