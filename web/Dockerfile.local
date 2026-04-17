FROM node:18-slim AS base

RUN apt-get update && \
    apt-get install -y --no-install-recommends python3 python3-pip && \
    pip3 install --break-system-packages flask gunicorn curl_cffi && \
    apt-get clean && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY parser-harness.js web/webapp.py ./

EXPOSE 5050

CMD ["gunicorn", "webapp:app", \
     "--bind", "0.0.0.0:5050", \
     "--workers", "1", \
     "--threads", "8", \
     "--timeout", "600"]
