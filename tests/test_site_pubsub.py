from janitor.site.pubsub import Subscription, Topic


async def test_publish_to_subscriber_that_does_not_read():
    topic = Topic("test")
    with Subscription(topic) as slow, Subscription(topic) as reader:
        for i in range(1000):
            topic.publish(i)
            assert reader.get_nowait() == i
        assert slow.qsize() == 100
        assert slow.get_nowait() == 900
