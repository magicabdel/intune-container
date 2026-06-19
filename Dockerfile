# Derived Intune image: the upstream base + a private virtual display (Xvfb).
#
# The Microsoft identity broker is a GTK app and refuses to start without *a*
# display. Baking in Xvfb lets the container run a private, in-memory display
# (:99) so background SSO works while the host's real screen is never bound in.
#
# Build it yourself, then push to your own registry and hardcode that URL as
# DEFAULT_IMAGE in src/init.rs:
#
#   just build-image                         # builds localhost/intune-container:local
#   docker tag localhost/intune-container:local <your-registry>/intune-container:latest
#   docker push <your-registry>/intune-container:latest
#
# The base can be overridden at build time:  --build-arg BASE_IMAGE=...
ARG BASE_IMAGE=ghcr.io/frostyard/ubuntu-intune:latest
FROM ${BASE_IMAGE}

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends xvfb \
 && rm -rf /var/lib/apt/lists/*
