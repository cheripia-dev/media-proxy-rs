FROM --platform=$BUILDPLATFORM rust:latest AS cross_build
ARG BUILDARCH
ARG TARGETARCH
ARG TARGETVARIANT
RUN apt-get update && apt-get install -y clang musl-dev pkg-config nasm mold git meson ninja-build xz-utils
COPY crossfiles /app/crossfiles
RUN bash /app/crossfiles/deps.sh

FROM --platform=$BUILDPLATFORM cross_build AS dav1d
RUN git clone --branch 1.5.5 --depth 1 https://github.com/videolan/dav1d.git /dav1d_src
RUN cd /dav1d_src && bash -c "source /app/crossfiles/meson.sh && meson build -Dprefix=/dav1d -Denable_tools=false -Denable_examples=false -Denable_tests=false -Ddefault_library=static --buildtype release --cross-file /app/crossfiles/cross.txt"
RUN cd /dav1d_src && bash -c "source /app/crossfiles/meson.sh && ninja -C build"
RUN cd /dav1d_src && bash -c "source /app/crossfiles/meson.sh && ninja -C build install"

FROM --platform=$BUILDPLATFORM cross_build AS lcms2
RUN git clone -b lcms2.16 --depth 1 https://github.com/mm2/Little-CMS.git /lcms2_src
RUN mkdir /lcms2
RUN cd /lcms2_src && bash -c "source /app/crossfiles/meson.sh && meson build --prefix=/lcms2 -Ddefault_library=static -Dfastfloat=true -Dthreaded=true --buildtype release --cross-file /app/crossfiles/cross.txt"
RUN cd /lcms2_src && bash -c "source /app/crossfiles/meson.sh && ninja -C build"
RUN cd /lcms2/ && cp /lcms2_src/build/src/liblcms2.a . && cp /lcms2_src/build/plugins/threaded/src/liblcms2_threaded.a . && cp /lcms2_src/build/plugins/fast_float/src/liblcms2_fast_float.a .

FROM --platform=$BUILDPLATFORM cross_build AS build_app
ENV CARGO_HOME=/var/cache/cargo
ENV SYSTEM_DEPS_LINK=static
WORKDIR /app
COPY avif-decoder_dep ./avif-decoder_dep
COPY .gitmodules ./.gitmodules
COPY --from=dav1d /dav1d /dav1d
COPY --from=lcms2 /lcms2 /lcms2
RUN cp -r /lcms2/* /dav1d/lib
ENV PKG_CONFIG_PATH=/dav1d/lib/pkgconfig
ENV LD_LIBRARY_PATH=/dav1d/lib
COPY src ./src
COPY Cargo.toml ./Cargo.toml
COPY Cargo.lock ./Cargo.lock
COPY asset ./asset
COPY examples ./examples
COPY napi ./napi
RUN --mount=type=cache,target=/var/cache/cargo --mount=type=cache,target=/app/target bash /app/crossfiles/build.sh

FROM build_app AS pkg
ARG UID="852"
ARG GID="852"
# ビルド成果物のスモークテスト。静的リンクされた musl バイナリは Debian 系の
# ビルダー上でもそのまま動くため、シェルの使えるここで実施する。
RUN bash -c './media-proxy-rs & for i in $(seq 1 30); do ./healthcheck && exit 0; sleep 1; done; exit 1'
# シェルの無い最終ステージでも config.json の自動生成 (cwd への書き込み) ができる
# よう、配置と所有者を先に用意しておく。RUN chown はビルドステージでしかできない。
RUN mkdir -p /out/media-proxy-rs && cp /app/media-proxy-rs /app/healthcheck /out/media-proxy-rs/ && chown -R "${UID}:${GID}" /out/media-proxy-rs

FROM scratch
ARG UID="852"
ARG GID="852"
# 静的リンクバイナリの唯一のランタイム依存は TLS ルート証明書。
# reqwest (rustls-native-certs) が /etc/ssl/certs/ca-certificates.crt を読む。
# ホストの CA ストア (社内 CA 等) を使いたい場合は運用側のバインドマウントで上書きできる。
COPY --from=build_app /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
# /out/media-proxy-rs をディレクトリごとコピー。--chown でトップのディレクトリ自体の
# 所有者も設定する (BuildKit は COPY 元のディレクトリ所有権を運ばない)。
COPY --from=pkg --chown="${UID}:${GID}" /out/media-proxy-rs /media-proxy-rs
WORKDIR /media-proxy-rs
USER "${UID}:${GID}"
EXPOSE 12766
# シェルが無いので HEALTHCHECK は exec フォーム。healthcheck は失敗時に非ゼロで終わる。
HEALTHCHECK --interval=30s --timeout=3s --start-period=15s CMD ["./healthcheck"]
CMD ["./media-proxy-rs"]
