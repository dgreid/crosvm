# Start docker container as root

```
docker run --privileged -it -v /dev/log:/dev/log -v /dev/kvm:/dev/kvm -v /run/user/$(id -u):/run/user/0 --mount type=bind,source="$(pwd)",target=/src crosvm:build /bin/bash
```
