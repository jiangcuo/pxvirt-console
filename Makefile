# Makefile for pxvdistream-client
include /usr/share/dpkg/default.mk

BUILD_DIR := build

# 需要复制的文件和目录
COPY_ITEMS := src debian Cargo.toml

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

	@GIT_SHORT_ID=$$(git rev-parse --short HEAD); \
	CURRENT_VERSION=$$(grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/'); \
	NEW_VERSION="$${CURRENT_VERSION}+$${GIT_SHORT_ID}"; \
	sed -i "s/^version = .*/version = \"$${NEW_VERSION}\"/" $(BUILD_DIR)/Cargo.toml; 

	@if [ "$(ARCH)" != "$(TARGET_ARCH)" ]; then \
		cd $(BUILD_DIR) && dpkg-buildpackage -us -uc -b -a$(ARCH) -Pcross; \
	else \
		cd $(BUILD_DIR) && dpkg-buildpackage -us -uc -b -a$(ARCH); \
	fi

