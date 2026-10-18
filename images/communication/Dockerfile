# compost's communication image: sends notifications (contract API 1, see
# docs/wiki/Notifications.md). Built on the server by `compost server deploy`.
FROM alpine:3.20
RUN apk add --no-cache ca-certificates curl jq
COPY communicate /usr/local/bin/communicate
RUN chmod 755 /usr/local/bin/communicate
USER nobody
ENTRYPOINT ["/usr/local/bin/communicate"]
