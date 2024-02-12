$(warning $(shell IMAGE_NAME=$(IMAGE_NAME) printenv | grep IMAGE_NAME))
$(warning $(shell IMAGE_TAG=$(IMAGE_TAG) printenv | grep IMAGE_TAG))
$(warning $(shell DOCKER_BUILD_ARGS=$(DOCKER_BUILD_ARGS) printenv | grep DOCKER_BUILD_ARGS))

ifndef IMAGE_NAME
#$(warning IMAGE_NAME is not set)
IMAGE_NAME=atlas-txn-sender
endif

ifndef DOCKER_BUILD_ARGS
# You can add '--no-cache --quiet' here if you like
DOCKER_BUILD_ARGS=
endif

ifndef IMAGE_TAG
IMAGE_TAG=$(shell git log -1 --pretty=format:"%h")
endif

all: build push

build:
	docker build $(DOCKER_BUILD_ARGS) -t $(IMAGE_NAME):$(IMAGE_TAG) .

push:
	docker push $(IMAGE_NAME):$(IMAGE_TAG)

latest: 
	docker tag $(IMAGE_NAME):$(IMAGE_TAG) $(IMAGE_NAME):latest
	docker push $(IMAGE_NAME):latest

exec:
	docker-compose run --rm --entrypoint /bin/bash atlas-txn-sender

release: build push latest
